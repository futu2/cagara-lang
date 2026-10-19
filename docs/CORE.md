# The typed core and the normalizer

This document states the language's compile-time semantics: what a program
means before any SQL exists. It is the reference for normalization, the
relational arena's constructors, and the parts of SQL semantics that the
language deliberately adopts.

Code references are to `crates/cagara-hir/src/`.


## 1. The pipeline

```text
source
  │  parse + resolve name occurrences
  ▼
resolve
  │  bidirectional type checking, one typed node per expression
  ▼
typed core          check/typed.rs: TypedBody, TypedExpr
  │  one compile-time normalizer
  ▼
normalization       normalize/: β-reduction, aliases, overloads, helpers
  │  query primitives construct relational nodes at saturation
  ▼
typed relational arena   normalize/arena.rs: CompiledQuery
  │  structural erasure
  ▼
Rel                 ir.rs: Rel, a backend-facing tree with no type metadata
  │  schema.schema (defensive), then SQL lowering and dialect rewriting
  ▼
SQL                 cagara-sql
```

The production boundary is `compile.rs::compile_definition`: it shares the
checker gate in `elaborate.rs::prepare_query_definition`, calls
`normalize::route::lower`, validates the erased relation, and records the
relation or its classified fault.

The pipeline golden snapshot pins the production relation, fault class,
diagnostics, and source locations.

## 2. The reduction rules

`normalize/mod.rs` holds the value domain; `normalize/value.rs` the two functions that
reduce it. `Normalizer::value` matches on the *expression*, `Normalizer::apply` on
the *value*. Nothing dispatches on the shape of an enclosing expression to
decide what an operation means.

```text
normalize (Field)        = a checked column
normalize (Lit str/int)  = a Label — the consuming operation says whether it is an
                      affix, a column name, a count, or a constant
normalize (Lit other)    = a checked literal
normalize (Name/Proj)    = a local binding, or the body of the definition it
                      resolved to
normalize (Lambda p… e)  = a closure over the environment
normalize (Case)         = a searched CASE template
normalize (Record/List)  = the arguments' values, with the record's field names
normalize (Sql s)        = a template awaiting its arguments
normalize (App h a…)     = foldl apply (normalize h) (map normalize-argument a…)

apply (Closure, v)          = bind the next parameter; normalize the body when
                              it was the last one
apply (Template, v)         = accumulate; build the checked expression when
                              saturated
apply (Stage s, v)          = accumulate; build the relational node when
                              saturated
apply (Join|Set c, v)       = accumulate; build the node when saturated
apply (Direction, v)        = a sort key
apply (Group, v)            = a grouping key
apply (Membership, v)       = accumulate; build `in` when saturated
apply (Fold, v)             = accumulate; build a frame bound or Frame
apply (Table|Label, v)      = a table leaf, or a symbol argument
apply (Overloads cs, v)     = the first candidate that applies
```

**A literal is a label until an operation says otherwise**
(`normalize/value.rs`, `value_at`). A string in a `symbol` position names an affix or
a removed column, an int in a count position is a count, and either in an
expression position is a constant. The reduction produces one value — `Label` —
and `scalar_of` turns it back into a checked expression where an expression is
wanted. That is why `prefix "u_"` and `limit 2` reduce without a rule of their
own.

**β-reduction with environments** (`normalize/env.rs`). An `Env` is a chain of frames,
innermost last; a closure captures the environment it was created in. Applying a
closure binds one more parameter, and the body is normalized when the last one
arrives. Nothing is ever substituted into syntax, so capture is impossible by
representation.

**Call by name, never memoized.** Each argument is normalized at the use site it
was written at (`Normalizer::argument`), so it keeps that site's typed node — its
type, row, origin and overload choices. Two uses of one definition therefore
produce two stage chains with two sites, which is what the checker recorded for
each.

**Sites.** `Normalizer` keeps a `Site` stack (`normalize/mod.rs`): every node of the
typed core the reduction is currently inside, outermost first, with the module,
definition and `ExprId` it came from. An expression that is another's *argument*
pushes a frame that is not `named`, so a stage record the checker made for a
lambda body is not mistaken for the enclosing application's. `stage_site` reads
the stack to stamp a constructed node with the application it belongs to.

**Aliases.** A name normalizes the body of the definition the checker resolved it
to (`Normalizer::name` → `Normalizer::definition`). An alias *is* its target, so
following it is that call rather than a separate `follow_alias` walk.

**Overload selection** (`Normalizer::overloads`). The checker records a `Choice`
per use site (`TypedExpr::choices`); a resolved choice is the one whose
definition is normalized. When the use site itself is still open, every candidate
is tried and the first that applies wins.

**Helpers.** A helper is an ordinary definition, so applying one is β-reduction:
its parameters are bound in the environment and its body is normalized under the
caller's context. Nothing copies a sub-graph into a caller: every use normalizes
the helper again, and all uses construct nodes in the same arena.

**Query primitives construct nodes at saturation.** A definition whose body is
`primitive "…"` normalizes to a *function value* — a stage accumulating its
arguments, a join or set accumulating its operands, a template accumulating its
expressions. When the last argument arrives, `apply` calls the arena
constructor, and that is the only place a relational node is made.

**Identity.** `id : a -> a = x => x` is removed by β-reduction where it is
applied. Where it is a value — the right operand of a composition — it is
recognised by one syntactic clause (`Value::is_identity`): a one-parameter lambda
whose body is its parameter, closing over no environment. The notes are in
`normalize/thunk.rs`.

## 3. The relational arena

`normalize/arena.rs`. Every query is a graph of `RelNode`s. A node records its kind
(the operation, not the spelling), its site (`NodeRef`: the definition and
expression that applied it, stable across inlining), its origin, its children in
operand order, its **row computed by the constructor**, and exactly one payload
shape for its kind. Erasure (`CompiledQuery::erase`) walks the graph emitting
`Rel`, stamping each node with `Rel::At(origin)`; it re-checks nothing.

The constructors are the single statement of relational validity:

| Constructor | Rule it enforces | Output row |
|---|---|---|
| `Builder::table` | the columns must be known | the table's columns |
| `Builder::rewrite` (`prefix`, `suffix`, `omit`, `mapValue`) | the affix/column/wrapper applies; `omit` of a missing column is a program error | computed by the operation from the input row |
| `Builder::keep_row` (`where`, `order`, `limit`, `offset`, `distinct`) | predicates are `bool` and row-phase and read only input columns; keys are row-phase and read only input columns; counts are non-negative | the input's row |
| `Builder::project` (`select`, `update`, `agg`) | fields placed by phase (`rules::Place`); `update` merges over the input row (right-wins); `agg` rejects `win`-phase fields | computed from the fields |
| `Builder::join` | side-qualified columns only (`rules::needs_side`), each present on its side; the predicate is row-phase `bool` | left columns, then the right's that the left lacks; nullable sides wrapped in `maybe` |
| `Builder::set` | both inputs expose the same columns, in the same order, with the same types | the left input's row |

Row formers are `core.rs::RowType` (`rename_all`, `omit`,
`map_value_nullable`, `overwrite`, `merge`); phase and side rules come from `rules.rs`, which the
checker's types and the IR validator already share, so a program cannot be
accepted by one and refused by the other. `schema.rs::schema` remains a
defensive validator over `Rel` for relations assembled outside the compiler; it
is not a second source of type information.

## 4. Semantics SQL makes observable

These are part of the language, not of the backend.

**Bag semantics.** Every relation is a bag. Stages map one-to-one over rows at
most (`where` filters, `order` reorders, projections map each row once), so no
stage deduplicates. `distinct` is the only deduplicator, and it deduplicates the
whole row.

Set operations (`ir::SetKind`, `cagara-sql/src/lower.rs::lower_set`):

| `SetKind` | SQL | duplicates |
|---|---|---|
| `Union` | `UNION` | removed |
| `UnionAll` | `UNION ALL` | preserved |
| `Intersect` | `INTERSECT` | removed |
| `Except` | `EXCEPT` | removed |

`Intersect` and `Except` are the standard, duplicate-removing forms: there is no
`INTERSECT ALL` or `EXCEPT ALL` in the language. Both operands must have the
same row (`Builder::set`), and the result keeps the left operand's columns and
order.

**Nullability.** `maybe a` is a value that may be `NULL` (see
[`NULLABILITY.md`](NULLABILITY.md)). Type variables in signatures mean non-null,
so `expr r a` does not accept `expr r (maybe a)`. Where nullability arises:

- a column declared `maybe`; the nullable side of a left/right/full join, whose
  types the checker writes as `mapValue (AsNullable) s` and the arena as
  `RowType::map_value_nullable`;
- `sum`, `avg`, `min`, `max`, `lag`, `lead` and the `*Over` windows return
  `maybe`, because SQL returns `NULL` for an empty or all-null input;
  `count`/`countOf`/`countDistinct` return non-null `int`.

Nullable values are not silently coerced. The prelude provides the operators:
`coalesce d x` (`sql "COALESCE($2, $1)"`) and infix `x ?? d`, `just`,
`nullIf`, `isNull`/`isNotNull`, `isTrue` (treats a nullable `NULL` as false), `whereTrue` (a filter whose
predicate is `isTrue`), `eqMaybe` (two `NULL`s count as equal). These are
ordinary templates, so their null behaviour is the SQL they name. A plain
comparison or aggregate input requires non-null operands, so the query states
where a `NULL` is allowed to flow.

**Phases.** `ir::Phase` is `Const | Row | Agg | Win`. `rules::mix` combines two
phases (`Agg` with anything but `Const`/`Agg` is a clash; `Win` combines with
`Row`/`Const` only) and `rules::nested` states the depth-1 rule for aggregate and
window arguments. Placement is `rules::place`: `where` predicates are `Row`;
`select` fields are `Row` or `Win` (a window may be computed in a `select`);
`agg` fields are `Agg` or `Const` — a `Win` field is refused, because the window
belongs in a `select` after the aggregation; `order` keys and window
partition/order keys are row-phase. The checker's types and the arena apply the
same rules, so aggregates cannot nest and windows cannot nest.

**Ordering.** `order [asc .x, desc .y]` takes `sortkey r` values; `asc`/`desc`
are primitives (`Prim::Asc`/`Prim::Desc`), so an operator spelling or an alias
is resolved by the primitive it denotes. `NULL`s sort last in either direction
(`cagara-sql/src/lower.rs::lower_order` sets `nulls_first: Some(false)`; window
order keys spell `ASC NULLS LAST` / `DESC NULLS LAST` in
`cagara-sql/src/stage.rs`). A constant key sorts nothing and is dropped.
`limit`/`offset` do not themselves order: a paged query is deterministic only if
it also says what order it is in, which the checker does not enforce.

**Windows.** A window function is a `win (expr r a)` template applied to a
window specification record (`{ partition = [..], order = [..], frame = .. }`) and
then to its expression arguments; the spec is the first argument and is consumed
by the template application in `normalize/scalar.rs`. Frames are built by `rows`/`preceding`/
`following`/the fixed bounds and validated by `checked::frame`.

## 5. The `sql` escape hatch

A definition whose body is `sql "…"` is the only way to write SQL that the
compiler does not construct. What is checked
(`elaborate.rs::validate_template_definition` → `check_placeholders`):

- placeholders are 1-based and stand alone (not inside a name or a string
  literal), are neither missing nor past the declared arity;
- the declared signature fixes the arity and the result kind
  (`normalize::declared_template_kind` → `check::core_facts`), which decides
  whether the template becomes a scalar, aggregate, or window expression.

What is **not** checked is the SQL body: no parsing, no type inference, no
dialect check beyond the rewrite the backend performs. A template is trusted
text with typed holes. That is a deliberate trade: the prelude is written this
way so that adding a function needs no Rust, and a template that names a
nonexistent SQL function fails at the database, not in the checker.

## 6. Failure modes

`normalize/mod.rs::Control` is the normalizer's only refusal, and its fault class is
what a caller reports:

- `Control::Unsupported { origin, what }` — no rule for an accepted construct.
  In total production this is a *compiler* fault
  (`Fault::Compiler`, reported as `internal error in …`).
- `Control::Program(Error)` — the rule applied and refused what it was given: a
  table whose columns the checker could not close, `omit` of a missing column,
  a non-boolean predicate, mismatched set-operation rows, a negative `limit`.
  That is a *program* diagnostic, blamed on the expression.

`normalize/budget.rs` bounds two shapes: `MAX_DEPTH = 200` definitions on one
normalization path (which also catches a cycle, reported as "definition refers to
itself"), and `MAX_WORK = 200_000` visited or constructed nodes. The work is
charged *before* the node is built (`Builder::charge`), so a refused normalization
discards its arena whole instead of returning a prefix of one. Exceeding either
is `Control::Unsupported`: the program is well-typed and the compiler could not
build it within its limits.
