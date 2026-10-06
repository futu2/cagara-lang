# Feasibility note: the checker already records what a full elaborator needs

Before starting the source-elaboration layer (review finding 1, plan step 2), I
checked the three cases I expected to force the elaborator to *re-derive* types.
Re-deriving them would create a second source of truth for types — the exact
failure this migration exists to remove — so each case had to be resolved by
*reading* checker facts, or the plan needed a checker change first.

**All three are already recorded.** Verified by probe on the current tree.

## Case 1 — overload instantiation

`helper = x => x + x` is checked once with holes and instantiated per use, so an
interior node's type depends on *which use*. `lookup` runs per use site with the
site's `ExprId` (`infer.rs:1216`, `self.lookup(n, e.id, sp)`), and
`Checker::uses` is drained *after* `check_def` returns and resolved then
(`infer.rs:52`), so the per-use instantiation is what gets recorded.

```cagara
people : query { n = int, x = float } = table "p" "people"
twice = x => x + x
q = people & select { a = twice .n, b = twice .x }
```
```
id=13  ty=Some(Int)      <- twice .n
id=16  ty=Some(Float)    <- twice .x
```

Two uses of one polymorphic helper, two distinct scalar types, both keyed by the
use's own `ExprId`. No re-derivation needed.

## Case 2 — join-side columns (`.<x` / `.>x`)

The concern was that a join predicate is checked as one `expr (join r s) bool`
node rather than per column. But the *value* type is what a `CheckedExpr` needs,
and it is recorded per `ExprId` (`infer.rs:988`,
`self.uses.push((e.id, sp, a.clone(), None))` — `a` is the column's value type).

```cagara
u : query { id = int, n = string } = table "p" "u"
v : query { id = int, m = float }  = table "p" "v"
q = u & innerJoin v (.<id == .>id)
```
```
id=13  ty=Some(Int)      <- .<id
id=14  ty=Some(Int)      <- .>id
```

## Case 3 — helpers with deferred constraints

`addPrefix = p => q => q & prefix p` leaves a `keyMap` row term for the use site
to bind. The concern was that `docs/ARCHITECTURE.md` calls for "a small pure
normaliser", i.e. re-implementing part of the evaluator.

In fact the checker already solves it: the use site's scheme comes out closed,
and the helper's own interior facts are recorded too.

```cagara
addPrefix = p => q => q & prefix p
r = addPrefix "u_" t
q = t & where (.a > 1) & select { x = .b }
```
```
id=21  ty=Some(Int)                                    <- .a  (inside the helper-driven query)
id=22  ty=Some(Int)                                    <- .a > 1
id=27  ty=Some(String)                                 <- .b

def 1 addPrefix  scheme=prefixAffix a -> query b -> query keyMap(a, b)
def 2 r          scheme=query { u_a = int, u_b = string }     <- closed at the use
def 3 q          scheme=query { x = string }
```

## Consequence for the plan

`TypeCheck::use_ty(module, ExprId)` plus `type_of`/`scheme_view`/`choices_of`
cover what the elaborator needs at both the node level and the definition level.
So plan step 2 is a **reading** layer, not a re-derivation:

* no second type computation, and therefore no second source of truth;
* `None` from `use_ty` means "the checker recorded no scalar here", which is
  exactly the `CheckedExpr` cases that are not scalar (`Query`, `Function`, open
  rows) — the elaborator must handle those explicitly rather than invent one;
* `Some(Unknown)` means a scalar the checker did not solve, which is a real
  answer and must stay distinguishable from `None`.

The remaining question the probe does **not** settle is which of the recorded
facts belong to which *stage*: a `select`'s field list and a stage's query
argument are both expressions, and the elaborator has to know the pipeline
structure, not only the types. That is what
`cagara_hir::rules::is_pipe_name` and the stage-operator declarations already
encode, so the elaborator reads the same table the evaluator does rather than
recognising stage names by hand.

## Outcome

The elaborator was built and covers **every query definition of every
example** — 9 in `report.cagara`, 1 in `public.cagara`, 1 in
`schema.cagara` — with **zero unsupported**. The source-level differential
test (`checked/tests.rs`, `assert_source_elaboration_agrees`) compares each
one against the evaluator's tree and treats an unsupported definition as a
failure rather than a skip, and additionally asserts that every definition
the evaluator produced a relation for was *considered* — so a definition
elaboration silently stopped seeing cannot reduce coverage unnoticed.

It also runs over eighteen stage combinations: where, select, update, omit,
prefix, suffix, order+limit, offset, distinct, inner and left join, a join
operator, agg, agg-then-where, a set operation, a window, a pipeline, and
the `&?`/`&-` shorthand form.

Mutation-checked: perturbing the `Omit` eraser makes it fail with
`omit: q elaborates from source to a different tree than the evaluator
produces`, which is the property the bridge-based harness could not have.
