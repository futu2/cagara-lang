# Row types

Cagara keeps query schemas in the type system. Rows are ordered fields with an
optional open tail, and `query r` carries a row `r`. The checker reduces row
operations when their inputs are known and retains them until an open row is
resolved.

## Operations

| Type term | Meaning |
|---|---|
| `keyMap m r` | Apply a key mapper to every field name in `r` |
| `mapValue w r` | Apply a value wrapper to every field type in `r` |
| `merge r s` | Overlay `s` on `r`; right-hand fields replace same-name fields |

Key mappers are first-order witnesses: `id`, `prefix "x"`, `suffix "x"`, and
composition. `prefix` and `suffix` read their literal argument at the call
site, so a helper can remain polymorphic in its mapper and row:

```haskell
addPrefix = p => q => q & prefix p
```

`mapValue` currently supports `nullable` and `list`. It is used by outer joins
to make columns on the missing side nullable. The wrappers are deliberately
first-order; Cagara does not expose higher-kinded type-level functions.

`merge` keeps the left row's field positions, replaces collisions with the
right row's types, and appends right-only fields. This is the row rule used by
`update` and joins.

## Constraints

Rows are kinded separately from ordinary types. `keyMap`, `mapValue`, and
`merge` produce rows; their inputs must therefore be rows (and key/value
mapper arguments must have the matching kind). Reduction uses the same schema
functions as IR validation, so type checking and SQL lowering cannot disagree
about names, order, or value wrappers.

Key and mapper arguments must be literals when the operation needs a concrete
witness. A collision or empty mapped name is an error. Open rows defer the
operation until their tail is known; no guessed columns are introduced.

The surface syntax is documented in [`LEARN.md`](LEARN.md), and the type
implementation lives in `crates/cagara-hir/src/check.rs`.
