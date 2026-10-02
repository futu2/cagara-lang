# Kinded rows: `mapKey` / `mapValue` at type level

A proposal to move `mapKeys` out of the checker's constraint solver and into the
type core, as a reducible row term. This document is a **design review**, not an
accepted plan: it reconciles the proposal against the shipped design in
`docs/KEYMAP-DESIGN.md`, records the contradictions, and lists the decisions
that must be made before any code changes.

Status: proposed. Nothing here is implemented.

---

## 0. Why this revision exists

`docs/KEYMAP-DESIGN.md` §0 and §5 reject literal-in-type encodings of key
mapping, and `docs/PLAN.md` item 25 records the reason as settled: encoding keys
as `Labels(Vec<String>)` or as a `keys` chain over `Ty::Lit`, then relating a
literal to `string` inside `unify`, makes `unify` **non-transitive**:

```
Lit "x" ~ string     ✓
string  ~ Lit "y"    ✓
Lit "x" ~ Lit "y"    ✗
```

The conclusion drawn was that key mapping cannot be a type former at all, and
`mapKeys` was implemented as two string-literal markers plus a `Cons::MapKeys`
constraint solved against a concrete row
(`crates/cagara-hir/src/check.rs:2300-2339`).

**The proposal in this document does not re-propose that encoding, and it does
not hit that failure.** The distinction is the whole reason this revision is
worth reviewing:

- The rejected design reflected *literal values* into `Ty` and then had to
  relate `Lit "x"` to `string` inside `unify`. Transitivity died there.
- This proposal keeps the mapper at a **separate kind** (`KeyMap`,
  `ValueMap`) whose inhabitants are **closed data**. A mapper is a value of
  kind `KeyMap`, never a type-level string. There is therefore no
  `Lit "x" ~ string` law anywhere, and Failure 1 does not arise.

A bounded witness set is categorically different from literal-in-type. §0 of
`KEYMAP-DESIGN.md` should be read as rejecting the latter only; this document
records that reading so the two do not appear to contradict.

---

## 1. Kinds

```
Kind ::= Type | Row | KeyMap | ValueMap
```

`query` and `row` are type constructors of kind `Row -> Type`:

```
Γ ⊢ r : Row                 Γ ⊢ r : Row
───────────────             ───────────────
Γ ⊢ row r : Type            Γ ⊢ query r : Type
```

Consequences worth stating plainly, because they are the proposal's main gain:

- `row r -> row s` is a function between values of kind `Type` — an ordinary
  function type. `select`'s projection argument is `row r -> row s`, so its
  argument is a **function**, not the record-of-expressions the current
  implementation takes.
- A bare `r -> s` is an ordinary HM type variable of kind `Type`. It does not
  silently become a row. (Today a bare row variable is `Ty::Var`, and
  `unify_rows` decides row-hood from the head of the type — there is no kind to
  consult.)

### Kind rules for the mappers

```
Γ ⊢ m : KeyMap          Γ ⊢ r : Row
──────────────────────────────────── mapkey m r : Row

Γ ⊢ v : ValueMap        Γ ⊢ r : Row
──────────────────────────────────── mapvalue v r : Row
```

These formation rules are the point of the kinding: `mapkey` applied to a
`ValueMap`, or a `Type` used where a `Row` is expected, is rejected **before**
unification runs. The proposal's framing — "the formation rules reject
mapper-axis mistakes before unification" — is accurate.

---

## 2. Row terms

```
RowTerm ::= closed { label = Type, ... }
         | open { label = Type, ... | r }
         | r
         | merge RowTerm RowTerm
         | mapkey KeyMapTerm RowTerm
         | mapvalue ValueMapTerm RowTerm
```

Rows are ordered and scoped: a repeated label may be introduced while building
a row and the rightmost binding is live.

`merge` keeps old field positions, replaces a colliding old value with the new
one, and appends labels that occur only in `new`:

```
merge { id=int, left=string } { id=string, right=int }
  = { id=string, left=string, right=int }
```

**This rule already exists in the repo**, as `schema::merge_columns`
(`crates/cagara-hir/src/schema.rs:205-219`), where it is the column rule for
`update`: keep the input's positions, replace matches, append names the input
lacks. The proposal promotes it from a per-stage column computation to a
general row operation. That promotion is the substantive change here — the rule
itself is not new.

---

## 3. Witnesses

```
KeyMap   = Id | Prefix string | Suffix string | Snake | Kebab | Camel
         | Replace string string | Compose KeyMap KeyMap
ValueMap = Id | maybe | List
```

For a closed row, mapping reduces field by field. Mapping never changes field
value types on the key axis and never changes labels on the value axis.

For an open row the hidden tail cannot be inspected, so the result is retained
as `mapkey m r` / `mapvalue v r` rather than guessing labels, and is normalized
once a later substitution makes the row concrete.

**This is the deferred-term design, and it is what makes the proposal
interesting.** The shipped implementation takes the opposite position: a
`mapKeys` over a rigid tail is a *hard error* — "`mapKeys` renames every column
and needs a closed input row; it cannot be used where the row is still open"
(`crates/cagara-hir/src/check.rs:2309-2319`) — and a variable tail returns
`Ok(false)`, meaning *wait*, not *defer a term*. `mapKeys` therefore cannot
appear inside a helper whose signature leaves the row open, which
`KEYMAP-DESIGN.md:197-198` lists as an accepted cost.

Carrying `mapkey m r` as a row term would lift that restriction. It is the main
thing the proposal buys, and it is expensive — see §6.

---

## 4. Prelude schemes as proposed

```
mapKey   : forall m:KeyMap. forall r:Row.
           keymapper m -> query r -> query (mapkey m r)
mapValue : forall v:ValueMap. forall r:Row.
           valuemapper v -> query r -> query (mapvalue v r)
merge    : forall r1:Row. forall r2:Row.
           query r1 -> query r2 -> query (merge r1 r2)
```

Instantiation freshens value, row, key-mapper, and value-mapper variables in
separate namespaces; overloads are finite sets of HM schemes, and an
application tests each case with a cloned substitution state and commits the
unique match.

The overload machinery described here **already exists**: `Cons::Overload`, the
`Choice` enum, and trial-unification-fitting with a trailed substitution
(`crates/cagara-hir/src/check.rs:186-208`, `:479`, `:670`). The freshening
namespaces are new; the fitting strategy is not.

---

## 5. Where the proposal contradicts the repo

These must be decided before implementation. They are not details — each one is
a semantic change to something already shipped and tested.

### 5.1 `ValueMap` is not a closed kind as specified

The proposal says "the witnesses are closed data, not arbitrary type-level
functions," and lists `ValueMap = Id | maybe | List`.

But `maybe` and `List` are **polymorphic type constructors**. Mapping
`maybe` over a row must send a field of type `t` to `maybe t`. That is the
type-level function `λt. maybe t` — an Fω type-level lambda, not closed data.
As written, `ValueMap` is inhabited by functions of kind `Type -> Type`, yet
`Kind ::= Type | Row | KeyMap | ValueMap` declares no arrow.

So the spec is internally inconsistent, and the gap is load-bearing: without an
answer, `mapValue` cannot be typed at all. The two ways out are §7.

Note `mapKey` is *not* affected. `Prefix string`, `Suffix string`, and
`Replace string string` are genuinely closed (`string -> string`
substitutions), and `Compose` closes over them. The key axis is first-order as
claimed; only the value axis is not.

### 5.2 Collisions: the proposal silently resolves what the repo rejects

Proposal: "A non-injective key map can produce the same label more than once;
`live` resolves that collision to the newer binding, just as `merge` does."

Repo: a collision is a **hard error** — `map_columns` returns "the key map would
produce column `{new}` twice"
(`crates/cagara-hir/src/schema.rs:266`), covered by
`map_keys_errors` (`crates/cagara-hir/src/check/tests.rs:977`).

Under the proposal, `mapKeys (Prefix "a")` over `{ id, name }` would silently
keep one of `a_id` / `a_name` instead of failing. The doc's stated rationale for
erroring — the asymmetry the doc itself states, "a typo is a compile error in one and a silent no-op in the other"
(`KEYMAP-DESIGN.md:237-240`) — applies with more force to silent collision than
to silent no-op, because a collision changes which column a later `.x` refers
to.

`live`-wins is defensible *if* derived-row semantics are wanted (it makes
`merge` and `mapkey` agree). It must be a deliberate choice, and if taken it
weakens the "every accepted query has a fully known row" claim at
`KEYMAP-DESIGN.md:157-163` — the row stays known, but the *provenance* of a
label becomes non-injective.

### 5.3 Two `merge`s with opposite argument order

Proposal: "Join rows use `merge right left`, so fields from the left input
overwrite duplicate fields from the right input."

Repo: `rules::join_columns` is left-first-wins — the left input's columns, then
the right's that the left does not have (`crates/cagara-hir/src/rules.rs:137-146`).

The *result* is the same (left wins). The *argument order* is inverted relative
to `merge`'s own definition, where the second argument's bindings are the live
ones. So `merge right left` is what produces left-wins — which means `merge`'s
first argument is the "old"/overwritten side. That is consistent, but it
inverts the reading of every other line in the spec, where `merge old new` takes
old first. Worth pinning in the doc before two spellings ship.

### 5.4 Surface schemes that do not match the implementation

The proposal's relational prelude is a **proposal**, not a description of the
repo. Differences found:

| Scheme | Proposal | Repo |
|---|---|---|
| `select` | `(row r -> row s) -> query r -> query s` | takes a record of expressions (`select {.a, .b}`); `Select | AggStage` build a `Cons::Update` (`check.rs:1882-1898`) |
| `where` | `(row r -> bool) -> query r -> query r` | matches |
| `table` | `string -> string -> query r` | matches (`check.rs:1870`) |
| `agg` | `(row r -> row s) -> query r -> query s` | a stage over grouped rows, not a row function |
| `join` | `query r -> (row l -> row r -> bool) -> query l -> query c` | `Join(kind)` with `Prim` arity 3 (`value.rs:79`); predicate is a record expression, and `.<x` / `.>x` are phase-checked via `Place::JoinOn` |

The mapping operators in the proposal are written against a surface with
`x => y =>` lambdas and `_+_` sections. This repo is Cagara (`.cagara`), and its
pipelines use `&` with stage shorthands declared in `prelude.cagara`. Assume the
intent is to port the **type core**, not the surface syntax.

---

## 6. What a port costs

Concretely, against this repo:

| Layer | Today | Under the proposal |
|---|---|---|
| `Ty` | `Var`, `Rigid`, `Gen`, `Con`, `Fun`, `Row`, `Empty` (`check.rs:58-70`) — **no kinds** | adds kinds; `Row`/`Empty` merge into a `RowTerm` with explicit extent |
| `unify_rows` | flattens, matches visible labels, binds open tails (`check.rs:806-848`) | also records deferred equality for opaque row terms |
| Row terms | none — row ops are computed eagerly by the checker | `merge` / `mapkey` / `mapvalue` as reducible terms, normalized on substitution |
| `mapKeys` | hard error on rigid tail; wait on variable tail (`check.rs:2309-2319`) | reduces when closed, defers when open |
| `Ty` variants banned by §5 | "no new variant — no `Lit`, no `HList`, no `Labels`, no `Renames`" (`KEYMAP-DESIGN.md:171-174`) | must be revisited: row terms are new type formers |
| Dependency | `regex` at check time, evaluated once so checker and lowerer agree (`KEYMAP-DESIGN.md:182-185`) | the reducer must produce the same names the lowerer computes, now through a normalizer |

Two risks not visible in the proposal:

1. **Termination.** The proposal calls the row fragment "the first-order,
   terminating part of an Fω design." Normalizing `Compose` chains and
   `merge` under an open row is where termination must be argued. `Compose f g`
   is fine on closed rows; the deferred case is a rewriting system on open
   terms and needs a measure.
2. **Checker/lowerer agreement.** The shipped design deliberately calls *the
   same function* from both sides so they "cannot disagree about the new names"
   (`check.rs:2322-2324`). Deferring normalization moves the name computation
   to a later point; the invariant must be restated in terms of the normal
   form, or the two can drift.

---

## 7. The `ValueMap` decision (open)

`mapKey` can ship independently. `mapValue` cannot be typed until this is
settled.

**(a) Restrict `ValueMap` to nullary constructors.** Keep
`Kind ::= Type | Row | KeyMap | ValueMap` as written, and make the inhabitants
closed: e.g. `Id`, `Null`. Then `mapvalue v r` maps each field's *type*
`t` to a fixed type, which is closed data as claimed, the core stays
first-order, and termination is easy.

- Cost: `maybe` and `List` — the two witnesses anyone actually wants — are
  gone. Value mapping with no nullability constructor is close to `Id` plus
  boxing, which may not justify the kind.

**(b) Add arrow kinds.** `Kind ::= Type | Row | KeyMap | ValueMap | Kind -> Kind`,
with `maybe : Type -> Type`, `List : Type -> Type`, and
`mapvalue v r` applying `v` to each field type.

- Gain: the useful witnesses, and a value axis symmetric with the key axis.
- Cost: this is no longer "the first-order part." It is Fω, and the
  termination and inference arguments get harder: type-level application must
  be normalized, higher-kinded unification enters, and the "row variables are
  ordinary type variables" simplification weakens. The proposal's own claim
  that the fragment is first-order and terminating would have to be dropped.

There is a third path worth naming, though it is not a kind fix: keep
`ValueMap` first-order by making each witness a *specific* wrapper
(`Maybe`, `ListOf`) rather than the constructor `maybe` — that is (a) with
better ergonomics, and it preserves the closed-data claim. It is listed here
because it may be the actual intent behind `Id | maybe | List`.

**Recommendation:** settle this before designing anything else, because it
decides whether the core is first-order or Fω, and that decision propagates
into `unify`, the normalizer, and inference.

---

## 8. Suggested order, if this proceeds

1. **Decide §7** (the `ValueMap` kind question). Everything else depends on it.
2. **Decide §5.2** (collision: error vs. `live`-wins) and **§5.3** (merge order
   spelling). Both are small decisions with wide consequences.
3. **`mapKey` only, as a deferred row term.** Add `Kind`, the `KeyMap`
   witnesses, and `mapkey m r` as a reducible row term; keep closed rows
   reducing eagerly to exactly today's column computation, so
   `map_keys_rewrites_every_name` keeps passing unchanged. Leave `mapValue` out
   until step 1 is answered. This is the smallest change that exercises the
   genuinely new idea — a bounded-witness type-level key map with no
   literal-in-type — and it produces an artifact to judge the rest of the
   design on.
4. Only then consider `merge` as a general row operation, extents, and the
   surface-scheme changes in §5.4.

Doing steps 1–2 before writing code is the point of this document. The shipped
design was reached after two rejected attempts (`PLAN.md` item 25); the value
of recording the reconciliation now is that the third attempt is judged on
whether it avoids Failure 1 — and it does — rather than on whether it restores
the machinery that was removed.

---

## 9. Formal reduction rules

These rules make the row term normalization precise and implementable.

### 9.1 Key mapper semantics

Each `KeyMap` witness is a string transformation `f : string -> string`:

```
eval(Id, s)              = s
eval(Prefix p, s)        = p ++ s
eval(Suffix x, s)        = s ++ x
eval(Snake, s)           = toSnakeCase(s)
eval(Kebab, s)           = toKebabCase(s)
eval(Camel, s)           = toCamelCase(s)
eval(Replace old new, s) = s.replace(old, new)  -- regex substitution
eval(Compose f g, s)     = eval(f, eval(g, s))
```

### 9.2 Row term reduction

Reduction is a partial function `reduce : RowTerm -> RowTerm` that fires when
enough structure is known:

**R-MapKey-Closed**: Map over a closed row field by field, collecting collisions
```
mapkey m { l₁=t₁, ..., lₙ=tₙ } 
  ⟹ { eval(m, l₁)=t₁, ..., eval(m, lₙ)=tₙ }  if all keys distinct
  ⟹ ERROR("duplicate key after mapping")     if any collision
```

**R-MapKey-Open**: Defer when the tail is still open
```
mapkey m { l₁=t₁, ..., lₙ=tₙ | r }  ⟹  mapkey m { l₁=t₁, ..., lₙ=tₙ | r }
  (no reduction; term is retained in normal form)
```

**R-MapKey-Compose**: Normalize compositions eagerly
```
mapkey (Compose f g) r  ⟹  mapkey f (mapkey g r)
```

**R-Merge-Closed**: Merge two closed rows
```
merge { l₁=t₁, ..., lₘ=tₘ } { k₁=s₁, ..., kₙ=sₙ }
  ⟹ { l₁=t'₁, ..., lₘ=t'ₘ, k'₁=s'₁, ..., k'ₚ=s'ₚ }
where:
  - For each lᵢ: if lᵢ ∈ {k₁..kₙ}, then t'ᵢ = sⱼ where kⱼ=lᵢ (right wins)
                otherwise t'ᵢ = tᵢ
  - k'₁..k'ₚ are the keys in {k₁..kₙ} not in {l₁..lₘ}, preserving order
```

**R-Merge-Open**: Defer merge when either side is open
```
merge { ... | r } s  ⟹  merge { ... | r } s  (no reduction)
merge r { ... | s }  ⟹  merge r { ... | s }  (no reduction)
```

### 9.3 Substitution and normalization

When a row variable `r` is bound to a concrete row `ρ`, every deferred term
mentioning `r` is re-reduced:

```
subst(r ↦ ρ, mapkey m r')     = mapkey m (subst(r ↦ ρ, r'))  then reduce
subst(r ↦ ρ, merge r₁ r₂)     = merge (subst(r ↦ ρ, r₁)) 
                                       (subst(r ↦ ρ, r₂))  then reduce
subst(r ↦ ρ, { ... | r' })    = { ... | subst(r ↦ ρ, r') }  then reduce if r'=r
```

Normalization applies reduction rules until a fixed point, and it is guaranteed
to terminate because:
1. `Compose` chains reduce by one level per step (structural decrease)
2. Substitution is well-founded (unification tracks occurs-check)
3. No rule introduces new row variables

### 9.4 Collision handling (decision required)

**Option A (error on collision)**: Keep the current behavior — `R-MapKey-Closed`
returns an error if `eval(m, lᵢ) = eval(m, lⱼ)` for any `i ≠ j`. This preserves
the "every label has known provenance" invariant.

**Option B (live-wins)**: Change `R-MapKey-Closed` to silently keep the
rightmost binding when keys collide. This makes `mapkey` consistent with
`merge`, at the cost of non-injective provenance.

**Recommendation**: Keep **Option A** for the initial implementation. The error
is more conservative, and the doc's own argument at §5.2 applies: a typo that
silently shadows a field is worse than a typo that fails to compile.

## 10. ValueMap resolution (decision required)

§7 identifies the core problem: `maybe` and `List` require `Type -> Type`, but
the proposed `Kind` grammar has no arrow.

**Recommendation**: Take the **third path** from §7 — keep `ValueMap` first-order
by making each witness a specific wrapper type, not a constructor:

```
ValueMap = Id | AsNullable | AsList
```

Then:
```
mapvalue AsNullable { l₁=t₁, ..., lₙ=tₙ }  ⟹  { l₁=maybe t₁, ..., lₙ=maybe tₙ }
mapvalue AsList { l₁=t₁, ..., lₙ=tₙ }      ⟹  { l₁=list t₁, ..., lₙ=list tₙ }
```

This gives the useful witnesses (`maybe`, `list`) without adding arrow kinds or
Fω complexity. The core stays first-order, termination is trivial, and
higher-kinded unification is avoided.

If more witnesses are needed later (e.g. `AsString`, `AsJson`), they can be
added to the closed set without changing the kind system.

## 11. Implementation roadmap

This is a concrete path forward, with each milestone independently testable.

### Milestone 1: Add kinds, no runtime change (1-2 days)

**Goal**: Introduce `Kind` and kind-check the existing `Ty` variants without
changing any runtime behavior.

**Tasks**:
1. Add `Kind` enum to `crates/cagara-hir/src/ty.rs`:
   ```rust
   pub enum Kind { Type, Row, KeyMap, ValueMap }
   ```
2. Add a `kind: Kind` field to `Ty::Var` and `Ty::Rigid`
3. Thread kinds through `fresh_var`, `fresh_rigid`, `instantiate`
4. Add `kind_of : Ty -> Kind` that computes the kind of a closed type:
   ```rust
   kind_of(Ty::Row(_)) = Kind::Row
   kind_of(Ty::Fun(_, _)) = Kind::Type
   // etc.
   ```
5. Add kind-checking to `unify`: before unifying `t1` and `t2`, assert
   `kind_of(t1) == kind_of(t2)`

**Acceptance**: All existing tests pass. No new functionality, but ill-kinded
terms are now rejected early.

### Milestone 2: Add KeyMap witnesses, keep eager evaluation (2-3 days)

**Goal**: Replace `Cons::MapKeys` with `KeyMap` witnesses and a `Ty::MapKey`
variant that reduces eagerly on closed rows.

**Tasks**:
1. Add `KeyMap` enum to `crates/cagara-hir/src/ty.rs`:
   ```rust
   pub enum KeyMap {
       Id, Prefix(String), Suffix(String), Snake, Kebab, Camel,
       Replace(String, String), Compose(Box<KeyMap>, Box<KeyMap>)
   }
   ```
2. Add `Ty::MapKey(KeyMap, Box<Ty>)` as a new row term
3. Implement `eval_key : (KeyMap, &str) -> String` per §9.1
4. Add reduction to `unify_rows`: when unifying `MapKey(m, row)` with anything,
   if `row` is closed, reduce it immediately via `eval_key` and unify the result
5. Update the prelude to declare `mapKey : keymapper m -> query r -> query (mapkey m r)`
6. Replace `check.rs:2300-2339` (the `Cons::MapKeys` solver) with type-level
   `Ty::MapKey` construction

**Acceptance**: `map_keys_rewrites_every_name` and `map_keys_errors` pass
unchanged. The new implementation produces identical column names to the old one
for every closed row.

**Verification strategy**: The key invariant is checker/lowerer agreement (§6).
To verify:
- Run `map_columns` (the lowerer's path) and the new reducer on the same input
- Assert they produce identical label sets for every test case
- A shared test fixture `test_key_transforms.json` can hold (mapper, input, expected) triples

### Milestone 3: Defer mapkey on open rows (2-3 days)

**Goal**: Allow `mapkey m r` where `r` is still open, carrying the term until
substitution makes `r` concrete.

**Tasks**:
1. Change `unify_rows` to accept `MapKey(m, r)` where `r` is open:
   - Do not reduce immediately
   - Record the term as part of the row's normal form
2. Add `subst_row : (Var, Ty) -> Ty -> Ty` that walks a row term and applies
   substitution, then re-reduces
3. Hook `subst_row` into the unification trail: whenever a row variable is
   bound, re-normalize any deferred `MapKey` terms mentioning it
4. Update the error message: remove "mapKeys needs a closed row" from
   `check.rs:2309`

**Acceptance**: The following now type-checks:
```cagara
let renameId : query {id:int|r} -> query {userId:int|r} =
  mapKey (prefix "user")
```

Previously this failed because `r` is open. Now it defers `mapkey` until the
caller instantiates `r`.

**Test cases**:
- `mapKey` over a closed row (milestone 2 behavior, unchanged)
- `mapKey` over a polymorphic row variable (new: defers)
- `mapKey` over a row with a tail variable, later unified with a concrete row
  (verify the deferred term reduces correctly after substitution)

### Milestone 4: Add merge as a row term (1-2 days)

**Goal**: Promote `schema::merge_columns` to a type-level `Ty::Merge` term.

**Tasks**:
1. Add `Ty::Merge(Box<Ty>, Box<Ty>)` to the row term grammar
2. Implement `R-Merge-Closed` from §9.2 in `unify_rows`
3. Add `merge : query r1 -> query r2 -> query (merge r1 r2)` to the prelude
4. Update `rules::join_columns` to construct `Ty::Merge` instead of computing
   the result eagerly

**Acceptance**: `join` type-checks with the new `merge` term, and the lowerer
produces the same column order as before.

**Note**: §5.3 identifies an argument-order discrepancy. Resolve it here by
making `merge`'s order match `rules::join_columns` (left-wins), and document the
choice.

### Milestone 5: Add ValueMap witnesses (optional, 2-3 days)

**Goal**: Implement `mapvalue` following the first-order design from §10.

**Tasks**:
1. Add `ValueMap` enum: `Id | AsNullable | AsList`
2. Add `Ty::MapValue(ValueMap, Box<Ty>)`
3. Implement `eval_value : (ValueMap, Ty) -> Ty`:
   ```rust
   eval_value(AsNullable, t) = Ty::Con("maybe", vec![t])
   eval_value(AsList, t) = Ty::Con("list", vec![t])
   ```
4. Add reduction rule to `unify_rows` (analogous to `MapKey`)
5. Add `mapValue : valuemapper v -> query r -> query (mapvalue v r)` to prelude

**Acceptance**: The following type-checks:
```cagara
let nullable : query {id:int, name:string} -> query {id:maybe int, name:maybe string} =
  mapValue asNullable
```

**Skip criterion**: If no one is asking for `mapValue`, defer this milestone. The
key axis (milestones 1-4) is independently useful.

### Milestone 6: Update surface schemes (as needed)

**Goal**: Reconcile the type schemes in §4 with the actual surface syntax.

This is a documentation and error-message task, not a core change:
- Update the prelude comments to show the kinded schemes
- If `select` / `agg` / `join` surface syntax changes, update their type schemes
  to match

**Acceptance**: The prelude's type comments accurately describe the kinded core.

## 12. Summary and recommendation

### What this design achieves

- **Solves the original problem**: `mapkey m r` works on open rows, lifting the
  "closed row required" restriction without hitting the transitivity failure.
- **Stays first-order**: `KeyMap` and `ValueMap` (as recommended in §10) are
  closed data, not arbitrary type functions. Termination is trivial.
- **Reuses existing logic**: The reduction rules for `mapkey` and `merge` are
  exactly `map_columns` and `merge_columns` from the repo, just promoted to the
  type level.

### Open decisions finalized

1. **ValueMap witnesses (§7)**: Use first-order wrappers (`AsNullable`, `AsList`)
   rather than arrow kinds. Preserves simplicity, gives the useful witnesses.
2. **Collision handling (§5.2)**: Keep error-on-collision for the initial
   implementation. More conservative, easier to relax later than to tighten.
3. **Merge argument order (§5.3)**: Make `merge r1 r2` be right-wins (the second
   argument's labels overwrite the first's), and document the choice clearly.

### Implementation path

Follow milestones 1-4 in order. Each milestone is independently testable and
leaves the system in a working state:
- M1: Add kinds (no behavior change, just earlier error detection)
- M2: `mapkey` on closed rows (same results as today, cleaner implementation)
- M3: `mapkey` on open rows (the new capability this design enables)
- M4: `merge` as a type term (promotes existing logic, no new semantics)
- M5: `mapValue` (optional, defer if not needed)

**Estimated total**: 8-12 days for milestones 1-4, assuming familiarity with the
checker and unification code.

### Risk mitigation

The two risks from §6 are addressed:
1. **Termination**: Argued in §9.3 — composition chains shrink, substitution is
   well-founded, no rule introduces variables.
2. **Checker/lowerer agreement**: Verified by milestone 2's acceptance criterion
   — the reducer and lowerer must produce identical names on a shared test suite
   before milestone 3 proceeds.

### Recommendation

**Proceed with milestones 1-3**. This delivers the core value (mapping over open
rows) with minimal risk, and it does so by promoting logic the repo already
trusts (`map_columns`, `merge_columns`) rather than inventing new rules.