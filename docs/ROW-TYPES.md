# Row types

Cagara keeps query schemas in the type system. Rows are ordered fields with an
optional open tail, and `query r` carries a row `r`. The checker reduces row
operations when their inputs are known and retains them until an open row is
resolved.

## Open rows in signatures

A row type may name the fields a helper needs and leave the rest open:

```haskell
byId : expr { id = int | r } int -> query { id = int | r } -> query { id = int | r } =
  i => q => where (.id == i) q
```

`{ id = int | r }` means “a row with an `int` column `id`, plus whatever `r`
turns out to be”; a call with a wider row instantiates `r` with the remaining
columns. The bare `query r` form says nothing about the row's fields, so a
helper that reads `.id` cannot be annotated with it — the checker reports “no
column `id`” because `r` is unconstrained. Name the field instead.

## Operations

| Type term | Meaning |
|---|---|
| `keyMap m r` | Apply a key mapper to every field name in `r` |
| `removeKey k r` | Drop the field named `k` from `r`; `k` is a label type (`symbol k`) |
| `mapValue w r` | Apply a value wrapper to every field type in `r` |
| `merge r s` | Overlay `s` on `r`; right-hand fields replace same-name fields |

`removeKey` takes its label as a type. A signature names it with `symbol k`,
which the call site binds to the literal: `omit "name"` binds `k` to `"name"`,
so `omit : symbol k -> query r -> query (removeKey k r)` gives the output row
with that column removed. A missing column is still reported by the `omit`
stage's constraint (`Cons::Omit`), not by the reducer.

`prefix` and `suffix` use the same label parameter, and their result names the
label-driven mapper built from it:

```haskell
prefix : symbol k -> query r -> query (keyMap (prependSym k) r)
suffix : symbol k -> query r -> query (keyMap (appendSym k) r)
```

The `k` bound at the call site is the same variable the result's mapper carries,
so the mapping cannot come from an unrelated witness. `prependSym k` /
`appendSym k` decode into an ordinary `prefix "s"` / `suffix "s"` mapper
(`labelPrefix`/`labelSuffix` in the type representation) once `k` is bound; while
`k` is still open — inside a helper — the term is retained exactly as a variable
mapper is.

A `symbol` position lifts a string literal to `Ty::Sym` rather than to
`string`, so a helper's parameter is generalized with its label intact and the
literal binds that same label at the use site — which is what lets the helper
stay polymorphic in both its affix and its row.

The stage a definition *is* comes from its own body, not from its result type:
each stage primitive's definition records its operation in the scheme it
publishes (`Scheme::stage`), so a use site names the operation it applied
without inspecting the type it produced. Recovering the stage from the result
is what used to break a body with two key stages; see
[`PLAN.md`](PLAN.md) under *Current behavior*.

`Cons::Omit` is the source of the missing-column error: it is the `omit`
definition's own obligation, instantiated per use, and the reducer tells it
"not yet" (an open row or an unbound label) from "not there" (a known label over
a closed row).

Key mappers are first-order witnesses: `id`, `prefix "x"`, `suffix "x"`, and
composition. `prefix` and `suffix` read their literal argument at the call
site, so a helper can remain polymorphic in its mapper and row:

```haskell
addPrefix = p => q => q & prefix p
```

`mapValue` supports three wrappers. The wrapper is a label in the type — the
signature is `mapValue : valueLabel w -> query r -> query (mapValue w r)`, so
the literal binds `w` on both sides exactly as `prefix`'s affix does. The names
are decoded by one table (`check/reduce.rs`, `value_map_of`):

| Wrapper | Effect |
|---|---|
| `"nullable"` | Wrap every field type in `maybe` |
| `"list"` | Wrap every field type in `list` |
| `"id"` | The identity mapper — no change |

`"nullable"` is the one outer joins use, to make the missing side's columns
nullable. The wrappers are deliberately first-order; Cagara does not expose
higher-kinded type-level functions.

`merge` keeps the left row's field positions, replaces collisions with the
right row's types, and appends right-only fields. This is the row rule used by
`update` and joins.

**None of these three is a query stage you can write.** They are *type-level*
row operations: the checker reduces them during inference, and the row they build
is what the enclosing stage's own constructors read. Writing one as a query body
type-checks and then fails at lowering with
``the relational primitive `__mapValue` is not lowered yet``, because there is no
relational operator for "change every column's type" — the type change is carried
by the checked row, not by a stage.

The user-facing spelling of `keyMap` is `prefix` / `suffix` and of `merge` is
`update`, which is why those are stages and these are not.

## Constraints

Rows are kinded separately from ordinary types. `keyMap`, `mapValue`, and
`merge` produce rows; their inputs must therefore be rows (and key/value
mapper arguments must have the matching kind). Reduction uses the same schema
functions as IR validation, so type checking and SQL lowering cannot disagree
about names, order, or value wrappers.

Key and mapper arguments must be literals when the operation needs a concrete
witness. A collision or empty mapped name is an error. Open rows defer the
operation until their tail is known; no guessed columns are introduced.

The user-facing row operations are documented in [`LEARN.md`](LEARN.md) —
`prefix`, `suffix` and `update` cover what `keyMap` and `merge` do. `mapValue`
has no surface spelling outside the prelude's own use of it. The type
implementation lives in `crates/cagara-hir/src/check/`: `row.rs` owns the row
algebra and its single reducer, `stage.rs` the typed stage nodes, `infer.rs` the
operations, `reduce.rs` the kind rules and the first-order reducers, and `ty.rs`
the representation.
