# Row types

Cagara keeps query schemas in the type system. Rows are ordered fields with an
optional open tail, and `query r` carries a row `r`. The checker reduces row
operations when their inputs are known and retains them until an open row is
resolved.

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

**Status and remaining work.** The label-driven mapper witnesses
`labelPrefix`/`labelSuffix` (built by `prependSym k` / `appendSym k`) exist and
decode once their label is bound. `prefix` and `suffix` still use the literal
`prefixAffix` / `suffixAffix` path. Moving them to labels needs one change
that is not yet made: a `symbol` parameter must take its stage from the result
type's mapper, so `key_stage` can handle `removeKey` and the label-driven
`keyMap` stages. Until then the prelude keeps `prefix`/`suffix` as they are.
`Cons::Omit` remains the source of the missing-column error; removing it needs
a fallible reducer.

Key mappers are first-order witnesses: `id`, `prefix "x"`, `suffix "x"`, and
composition. `prefix` and `suffix` read their literal argument at the call
site, so a helper can remain polymorphic in its mapper and row:

```haskell
addPrefix = p => q => q & prefix p
```

`mapValue` supports three wrappers. Read from the checker
(`check/infer.rs`, the `wrapper.as_str()` match):

| Wrapper | Effect |
|---|---|
| `"nullable"` | Wrap every field type in `maybe` |
| `"list"` | Wrap every field type in `list` |
| `"id"` | The identity mapper — no change, handled before the reduction |

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
implementation lives in `crates/cagara-hir/src/check/` (`infer.rs` for the
operations, `reduce.rs` for the kind rules, `ty.rs` for the representation).
