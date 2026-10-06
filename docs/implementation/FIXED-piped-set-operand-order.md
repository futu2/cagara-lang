# Fixed: piped set-operation operand order was reversed

**Status:** fixed in `prelude.cagara`. Found during the checked-core migration
by `elaborator`, verified independently by `lead`, and confirmed by the user as
a bug worth fixing rather than merely documenting.

## The bug

```cagara
users : query { id = int } = table "public" "users"
vips  : query { id = int } = table "public" "vips"
q = users & except vips
```

emitted

```sql
SELECT ... FROM (SELECT id FROM public.vips EXCEPT SELECT id FROM public.users) AS t1;
```

The operand written **second** (`vips`) became the **left** input, so
`users & except vips` computed `vips EXCEPT users` — the complement of the
query the user wrote. Demonstrated on SQLite with `users = {1,2}`,
`vips = {2,3}`:

| query | result |
|---|---|
| `users EXCEPT vips` (intended) | `1` |
| `vips  EXCEPT users` (emitted) | `3` |

Silently wrong rows, with no diagnostic. `docs/LEARN.md:921` promised the
opposite ("the observable result follows the left query"), so the
implementation contradicted its own documentation.

`union`, `unionAll`, and `intersect` were reversed too, but they are
commutative, so the reversal was invisible and hid the defect.

## Why the bare `&` could not express it

```text
prelude.cagara:37   _&_ : a -> (a -> b) -> b = x => f => f x
```

`users & except vips` desugars to `_&_ users (except vips)`. `except vips` is
already a partial application of the 2-argument primitive, so the pipe supplies
`users` as its **second** argument and `prims::call` sees `[vips, users]`:

```rust
Prim::Set(kind) => {
    let left = query(next())?;   // <- vips, written second
    let right = query(next())?;  // <- users, the piped input
```

A single curried definition cannot fix this: the piped form passes
`[vips, users]` and the direct form `[users, vips]` — the same two-element
shape requiring opposite indices. Re-parameterising the primitive would simply
invert the direct form instead of removing the bug. `elaborator` proved this by
testing the obvious sketch.

## The fix

Set operations became **stage operators**, alongside `&?`, `&=`, `&+`, `&*`,
`&.`, `&-`. Each is a separate binding that takes the piped query last, so
`&`'s application lands it on the left:

```haskell
infixl 1 &|      # union
infixl 1 &!      # unionAll
infixl 1 &^      # intersect
infixl 1 &~      # except

_&|_ : query r -> query r -> query r = q => r => union q r
_&!_ : query r -> query r -> query r = q => r => unionAll q r
_&^_ : query r -> query r -> query r = q => r => intersect q r
_&~_ : query r -> query r -> query r = q => r => except q r
```

The named functions (`union`, `unionAll`, `intersect`, `except`) and their
primitive bindings are **unchanged**, so the direct form still means what it
always did and `prims.rs` still reads `left` then `right`.

Symbolic spellings were required: operator spellings are symbol-only tokens
(`syntax_kind.rs::is_op_symbol`), so a word-based `&union` cannot be declared.
An earlier attempt using `&union`, `&unionAll`, `&intersect`, `&except` was
rejected by the parser and left the prelude unparseable until reverted.

## Verification

* `users &~ vips` and `except users vips` now emit **byte-identical** SQL.
* Both forms put the written first operand on the left.
* Pinned by `crates/cagara-sql/src/tests.rs`:
  `a_piped_except_is_not_reversed` and `every_set_operation_has_a_pipe_stage`.
* **Mutation-tested**: reverting the prelude binding to `except r q` makes both
  tests fail, and the failure message prints the reversed SQL. The tests
  therefore fail for the stated reason rather than passing vacuously.
* `docs/LEARN.md` §11 updated to the new spellings and to state plainly that
  the two forms agree.

## Why no example caught it

`union`/`unionAll`/`intersect`/`except` appeared in `cagara-sql`'s tests only in
the **direct** form `union a b`, and no `examples/*.cagara` used a set operation
at all — so none of the 40 frozen golden files exercised the piped form either.
A swap of the operands was invisible to the entire suite. The new tests close
that gap for all four operations.

Note this is the same class of blind spot as the nine inverted curried
arguments in `prims.rs` found during the same migration: an argument
*permutation* type-checks, because `Prim::call` takes a `Vec<Value>`.
