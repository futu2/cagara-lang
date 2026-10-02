# Key mapping: a redesign

Two stages, one of which is not really a special case at all. This document
replaces the earlier designs that put string literals into types or split key
mapping across half a dozen primitives; §0 records why those were wrong.

---

## 0. What the earlier designs got wrong

**Failure 1 — literal values in types broke unification.** Encoding keys as
`Labels(Vec<String>)`, or as a `keys` chain over `Ty::Lit`, and then relating a
literal to `string` inside `unify`, made `unify` **non-transitive**:

```
Lit "x" ~ string     ✓
string  ~ Lit "y"    ✓
Lit "x" ~ Lit "y"    ✗
```

Two types can each equal `string` while not equalling each other, so the
relation is neither an equivalence nor a congruence — and inference, overload
fitting, and constraint solving all assume `unify` is one. Subsumption is a
preorder; it does not belong inside `unify`.

**Failure 2 — one primitive per operation.** `replace`, `prefix`, and `suffix`
are the same thing with different arguments. Three primitives meant three
constraint kinds, three solve arms, and three IR nodes for one idea.

**Failure 3 — multi-key operations forced a bespoke structure.** A list of keys
is heterogeneous, so typing it needed `Labels`/`Renames`, or a heterogeneous
list plus a singleton element type plus a subsumption rule. A large base for a
small feature.

**The fixes were to take one key at a time** (so one operation is a row equation
`unify` already solves) **and to generalize the renamer** (so prefix, suffix,
and rename are one rule).

---

## 1. Surface

Two stages, subject-last like every other stage. Arguments are **string
literals**, read at the application.

```haskell
public_users = schema.users & omit "password_hash"
prefixed     = users & mapKeys "^" "u_"
suffixed     = users & mapKeys "$" "_v2"
renamed      = orders & mapKeys "^id$" "order_id"
stripped     = users & mapKeys "^user_" ""
```

```
omit    : string -> query r -> query out
mapKeys : string -> string -> query r -> query out
```

`prefix`, `suffix`, and `replace` are not primitives: they are `mapKeys` with
different arguments, which is the whole point.

| Want | `mapKeys` pattern → replacement |
|---|---|
| prefix `u_` | `^` → `u_` |
| suffix `_v2` | `$` → `_v2` |
| rename `id` to `user_id` | `^id$` → `user_id` |
| strip a prefix | `^user_` → `` |

Gone: `pick`, list-form `omit`, `rename`, `replace`, `only`, `drop`, `keyMap`,
`prefix`, `suffix`, the `mapper` type, `opaque`, `Labels`, `Renames`, `keys`,
and the heterogeneous-list and literal-type proposals.

`update` and the `{.x}` field shorthand stay. `select {.a, .b}` remains the way
to pick columns — it needs no key mechanism, because a field list is syntax.

---

## 2. `omit` — pure row unification, no new rule

`omit "k"` states one equation:

```
input ~ { k : t | output }
```

`output` is the **tail**. Unifying a concrete row against a pattern with a fresh
tail binds that tail to everything left over, in the input's order — that is the
existing leftover rule in `unify_rows`, not new code.

```
input   { id : int, name : string, age : int }
r ~ { name : t | out }
        t   = string
        out = { id : int, age : int }      ← leftover rule, input order kept
```

What comes for free:

- **Missing key.** The key is present on one side only and the other side is
  closed, so the existing `missing()` path reports `` no column `k`; available:
  ... `` — no new code.
- **Order.** The leftover row is built from the input's order, so positions are
  preserved.
- **Tail.** An open input keeps its tail: `out` is the leftovers over the
  input's tail.
- **Unknown input.** If the input row is not yet resolved, the constraint waits,
  like every other deferred constraint.

So `omit` adds a constraint kind to the checker and **nothing at all to
`unify`**.

---

## 3. `mapKeys` — one label-rewriting rule

`mapKeys p r` applies a pattern → replacement rewrite to **every** column name:

```
new_name(c) = regex_replace(p, r, c)
```

- A column the pattern does not match keeps its name unchanged.
- Order, types, and the input's tail are preserved.
- **A closed input row is required.** With an open or unknown tail the rewrite
  is undefined for the unknown columns. A variable tail waits; a rigid tail is
  an error.
- **An empty result is an error** (`key map leaves column `x` with an empty
  name`). Dropping is `omit`'s job, not a side effect of renaming.
- **A collision is an error** — two columns mapping to the same name.

This is the one operation that *invents* labels, and therefore the one genuine
special case in the feature. It is also the only operation that needs the
compile-time regex engine (§5).

---

## 4. Where the keys come from

The pattern, the replacement, and `omit`'s key are string literals **read from
the application** into the constraint:

| Stage | Recovered from |
|---|---|
| `omit "k"` | the argument literal |
| `mapKeys "p" "r"` | both argument literals |

A type-level literal was the alternative, and §0 is why it is out: the relation
between a literal type and `string` is subsumption, and subsumption inside
`unify` destroys transitivity. Reading the literal at the application keeps the
name where it actually is — in the syntax — and leaves `unify` untouched.

The mechanism is one rule in the checker: a key parameter is marked with a
dedicated marker type, and an application whose parameter resolves to that
marker must supply a string literal, whose value is recorded in the constraint.
The rule is stated once, not per stage name.

**A computed key is a type error.** `omit k` for a variable `k` cannot name a
column at check time, and neither can a computed pattern. Erroring is
deliberate: the earlier design's "not statically known → leave the output row
unconstrained and let the IR validator sort it out" is itself a special case (an
`opaque` payload tag, an unconstrained output row, and a second place where keys
are validated). Refusing means every query the checker accepts has a fully known
row.

---

## 5. What the base gains — and what it does not

| Layer | Change |
|---|---|
| `unify` | **nothing** |
| `unify_rows` | **nothing** |
| Subsumption / `coerce` | **nothing** |
| `Ty` | **no new variant** — no `Lit`, no `HList`, no `Labels`, no `Renames` |
| `cagara-hir` | gains a `regex` dependency, used at check time to compute the rewritten names |
| Type checker | key capture at the application; two constraint kinds (`Omit`, `MapKeys`) and their solve arms |
| IR / schema / lowering | one `Rel` node per stage and its column computation — the same pattern as `update` |

There is no mapper type, no mapper payload vocabulary, no `opaque`, and no
unconstrained output row.

The regex engine is the one new dependency. It is needed because the checker
must compute the output row to type anything downstream of a `mapKeys`, and the
rewrite must be evaluated exactly once, at check time, so the checker and the
lowerer cannot disagree about the resulting names.

---

## 6. Trade-offs

- **No reusable key values.** A key cannot be a parameter: `keep = omit "x"` is
  fine as a definition, but there is no `omit k` for a variable `k`. That is the
  price of keeping names out of types, and it is why `select` and `update`
  remain the tools for reusable transformations.
- **`mapKeys` is a real special case** — it invents labels, so it cannot be a
  row equation. §3 confines it to one rule.
- **`mapKeys` needs a closed row**, so it cannot be used inside a helper whose
  signature leaves the row open.
- **An empty replacement is an error rather than a drop**, keeping renaming and
  projection distinct.
- **A computed key or pattern is rejected** rather than deferred.

---

## 7. As built

The implementation settled the four questions this section used to hold:

1. **Name — `mapKeys`.** It is a stage, not a value, so it does not read as
   taking a mapper.
2. **Argument order — pattern first:** `mapKeys "<pattern>" "<replacement>"`.
3. **No pipeline shorthands.** The stages take long names, like `select` and
   `update` before the `&=` shorthands existed; one can be added later without
   touching Rust.
4. **`omit` and `mapKeys` each have their own `Rel` node**, mirroring `select`
   and `update`.

Still genuinely open:

- **A computed key or pattern is refused** (§4). If a use case appears for a key
  chosen at run time, it needs a decision about where the row stops being known,
  and that is a design change rather than an extension.
- **`omit` off a `DISTINCT`** wraps a derived table, matching `select`. Whether
  the projection or the dedupe should come first is a semantics question that
  has not been exercised by a real query.

### Considered and rejected: an empty replacement drops the column

Folding the drop into `mapKeys` — `mapKeys "^k$" ""` removes `k`, and a pattern
matching several columns removes all of them, so one stage would do both jobs
and gain pattern-drop — was rejected after measuring the two differences:

- **`omit` is row-polymorphic; `mapKeys` is not.** `query { id = int | r } ->
  query r` type-checks with `omit` and is refused with `mapKeys`, which needs a
  closed row. Every helper that works on any row containing a given column
  depends on this.
- **A typo is a compile error in one and a silent no-op in the other.**
  `omit "nmae"` reports ``no column `nmae`; available: ...``, while
  `mapKeys "^nmae$" ""` compiles and changes nothing. A mistyped drop would
  silently alter the row's shape.
- It could not be sugar in any case: `omit "k"` needs the derived pattern `^k$`,
  and a derived pattern is a *computed* pattern, which §4 refuses precisely so
  that every accepted query's row is known. Folding would mean deleting `omit`,
  not layering it.

The accepted cost: dropping is single-key, so `mapKeys "^internal_.*$" ""` is
not a way to drop a family of columns.
