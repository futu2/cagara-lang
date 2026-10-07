# The checked core: migration notes

This document records the first steps of the migration described in
[`ARCHITECTURE.md`](ARCHITECTURE.md). It is a status note, not a target: it
says what exists now, what it is for, and what has deliberately *not* been
changed yet.

## Where the pipeline is

Before:

```text
source -> syntax/AST -> type checker -> Evaluator -> Value -> Rel
                                  \-> schema validation -> SQL lowering
```

After this step:

```text
source -> syntax/AST -> type checker -> CheckedProgram
                                            |  (checked definitions, choices, diagnostics)
                                            v
                                       Evaluator -> CoreTerm -> Rel -> SQL lowering
```

The kernels are additive. `CheckedProgram` is built from the *same* type check
the evaluator already used, so the evaluator can consult it instead of
re-deriving what the checker learned; `CoreTerm` is a by-product of evaluation
rather than a replacement for it; and `Rel` is still the only thing the SQL
backend sees.

## 1. `CheckedProgram`

```rust
pub struct CheckedProgram {
    pub modules: Vec<CheckedModule>,
    pub diagnostics: Vec<Diagnostic>,
}
```

One coherent input for later phases. Before, three things had to be consulted
together and kept in step by hand:

| Question | Used to be |
|---|---|
| What type does `m:i` have? | `TypeCheck::type_of` |
| Which overload did this use pick? | `TypeCheck::choice` (evaluator reads it *during* evaluation) |
| What columns does the module's completion probe see? | `TypeCheck::probe_fields` |
| What did the type of this use print as? | `TypeCheck::use_type` |
| What went wrong? | `TypeCheck::errors`, plus `Workspace::diags` |

`CheckedProgram` holds one `CheckedModule` per loaded module, each with its
checked definitions (`CheckedDef`: name, definition index, scalar scheme, one
`CoreTerm` per assignment of the definition's overload holes, resolved choices
and printed types per instantiation), its probe fields, its use-site types, and
its diagnostics. `CheckedProgram::diagnostics` is the flattened list, so a
caller no longer concatenates `Workspace::diags` with `TypeCheck::errors` by
hand in the order it happens to remember.

`CheckedProgram::of(&Workspace) -> CheckedProgram` is the entry point. It runs
the existing checker and groups its output per module; it does **not** change
what is checked or in what order.

The type checker's internal representation (`Ty`: inference variables, rigids,
schemes, open rows) is not part of this type. `ScalarType` is: a small, closed,
printable mirror (`Int`, `Float`, `String`, `Bool`, `Decimal`, `Date`,
`Timestamp`, `Maybe(..)`, `List(..)`, `Unknown`) produced once, at the
boundary, by `check::ty_to_scalar` / `check::ty_fields`. Later phases therefore
do not need to know what an inference variable is.

## 2. `CoreTerm` and elaboration

`CoreTerm` is the intermediate representation the evaluator produces:

```text
Value::Record/List/Lit   ->  CoreTerm::Lit / Record / List
Value::Query(Rel)        ->  CoreTerm::Table | Where | Select | Update | Omit
                             | Prefix | Suffix | Agg | Order | Limit | Offset
                             | Distinct | Join { kind, on } | Set { kind }
Value::Expr(IR Expr)     ->  CoreTerm::Col | ExprLit | Tpl | Agg | Win | In | Group
Value::Dir/Frame/Bound   ->  CoreTerm::Dir | Frame | Bound
```

Every relational primitive has a *named constructor* on `CoreTerm`
(`CoreTerm::where_(input, pred)`, `CoreTerm::select(input, fields)`,
`CoreTerm::join(kind, left, right, on)`, …). The evaluator calls the
constructor, not a runtime lookup:

```rust
Prim::Where => CoreTerm::where_(query(next())?, lift(next())?),
```

instead of

```rust
Prim::Where => Value::Query(Rel::Where(Box::new(query(next())?), pred)),
```

The difference is the point of the whole exercise: in the second form the
*only* record that a `where` was built is a `Rel` node, and every semantic rule
about it has to be re-derived later by a walk over that node. In the first form
the constructor is where the rule is stated, the constructor returns
`Result`, and the node records that it was applied.

`CoreTerm` is a plain owned tree. It contains no closures, no environments, and
no partial applications: user lambdas are still run by the evaluator, and what
survives into `CoreTerm` is the saturated application. This is the smaller
half of "elaboration replaces evaluation" — it removes the dynamic *discovery*
of relational constructors (the `Prim` table) without yet removing the
interpreter that supplies their arguments.

## 3. The checked relational layer

```rust
pub struct CheckedQuery { pub row: RowType, pub node: CheckedQueryNode, pub origin: Origin }
pub struct CheckedExpr  { pub phase: Phase, pub ty: ScalarType, pub node: CheckedExprNode, pub origin: Origin }
```

`CheckedQueryNode` / `CheckedExprNode` mirror the corresponding `CoreTerm`
variants, but a `CheckedQuery` also carries the row it produces (not just the
row its input produced) and an `Origin { module, span }`.

Every constructor is a function returning `Result` that enforces the rule for
its stage:

| Constructor | Rule it states |
|---|---|
| `where(q, pred)` | `pred` is row-phase `bool`; output row = input row |
| `select(q, fields)` | fields are row-phase and name input columns only; output row = the field list |
| `update(q, fields)` | fields are row-phase and name input columns; output row = fields **overwritten onto** the input row (right-wins; see below) |
| `agg(q, fields)` | fields are `agg`- or `const`-phase; output row = the field list |
| `order(q, keys)` | keys are row-phase |
| `join(kind, l, r, on)` | `on` is row-phase; `.<x` names a left column, `.>x` a right column, a bare `.x` is rejected; the far side of an outer join is `maybe` |
| `set(kind, l, r)` | both inputs expose the same row |
| `group(e)` / aggregate templates | arguments are row-phase (the depth-1 rule) |

`update` says *overwritten*, not "merged", on purpose: two row formers exist and
they disagree on collisions. `RowType::merge` is the **join** law (left-wins;
`rules::join_columns`), while `RowType::overwrite` is the `merge r s` row former
the checker reduces for `update` and `Cons::Update` (right-wins; see
`check/reduce.rs`). Using the join law for `update` silently kept the old column
type. The distinction is documented on both functions in `core.rs`.

Phase and join-side rules come from `crate::rules` — the module the checker's
*types* already use — so this is one statement of the rule, not a third one.
Column-level rules are *modelled on* `crate::schema`, which is also what the SQL
backend's validation uses. Note two gaps that follow, both deliberate:

* `omit` re-states its rule rather than calling `schema::omit_columns`; the
  behaviour and the wording match, but it is a second copy of the text.
* `schema` is not a mere backstop in production: `eval.rs` calls
  `schema_located` on the evaluated (erased) path, so it is still *the*
  column validator there. It becomes a debug assertion only when the
  elaboration rung makes the checked path the production path.

Diagnostics carry an `Origin`, so an error names the stage that created the
invalid node instead of a location attached afterwards by whichever phase
noticed. A `Diagnostic` is a `Diag` (file, line, column, message) plus the
module/definition it belongs to.

## 4. One erase

```rust
pub fn erase(query: CheckedQuery) -> Rel
```

Erasure is total and structural: a `CheckedQueryNode` maps to the `Rel` variant
of the same name, and its `Origin` becomes the transparent `Rel::At` wrapper
the SQL lowerer already expects. Nothing is re-checked during erasure and
nothing is inferred.

**What "already valid" does and does not mean.** A `CheckedQuery` that exists
has a `row` that is consistent with its `node` — and this is now enforced by
the compiler rather than by convention. `CheckedQuery`/`CheckedExpr` have
private fields, their node enums are `pub(crate)`, and the public surface is
accessors (`row()`, `phase()`, `ty()`, `origin()`) plus the `QueryKind` /
`ExprKind` discriminants. Forging a value fails to compile
(`E0451` private fields, `E0603` private type); before, it compiled, so the
guarantee rested on nobody trying. The one exception is
[`from_rel_unchecked`], named to say so: it rebuilds a query from a `Rel`
without running the constructors, and is for tests and migration only.

That is the **name-level** invariant: *which columns*, in *what order*. It is
**not** a proof of the column types. `schema::schema` returns `Vec<String>`, so
a comparison against it cannot see a wrong `ScalarType`, a missing `maybe` on an
outer-join side, or a wrong `update` precedence — and neither can any test built
on that comparison. Type-level correctness rests on the `CheckedQuery.row`-level
tests (which do carry `ScalarType`) and on the SQL golden tests. An earlier
`update` defect that kept the old column type is the worked example: the
`erase`/`schema` invariant did not catch it; the row-level test did.

Erasure happens on both sides of the current boundary, through two functions
with deliberately different scopes:

* `checked::erase(CheckedQuery) -> Rel` erases a *checked* tree. This is the one
  the design is about, and it is total because a `CheckedQuery` carries strictly
  more information than a `Rel` (a row and an origin per node).
* `core_term::erase_core(CoreTerm) -> Rel` erases the *evaluated* tree, which is
  what the evaluator produces while it is still the production path.

The two agree on `Rel::At` placement: both stamp the node's origin and both
suppress double-wrapping, and `CoreTerm::of_checked` stamps the same location.
Having both is deliberate — the evaluator cannot yet hand back a
`CheckedQuery`, so the erased-IR contract is what keeps the SQL backend and the
cli/lsp integration unchanged.

The invariant that ties the two layers together is checked in tests:

```text
schema::schema(&erase(q)) == names(q.row)      # names, not types
```

so a checked query that erases to `Rel` disagreeing with its own recorded
*columns* is a test failure rather than a wrong SQL statement. The reverse
direction is inherently partial: a `Rel` does not record a stage's input row,
so `from_rel` must be told which projection it is looking at (`StageHint`) and
must fail rather than guess where it cannot know.

## 5. What was deliberately not changed

* **`Workspace` is not split.** It is still the mutable module loader and cache.
  `CheckedProgram::of` takes a `&Workspace` and reads it.
* **SQL lowering is not redesigned.** The lowerer still consumes `Rel` and
  still owns naming, CTEs, and dialect rewriting. It receives only erased IR
  and needs to know nothing about row types or inference — which is already
  true today, and is now explicit rather than accidental.
* **The CLI and LSP are unchanged.** They call the same `check`,
  `root_queries_checked`, and `compile` entry points, with the same
  diagnostics and the same exit codes.
* **`schema` is not deleted.** It stays the backend's guard rail and, for now,
  the second opinion that the checked layer is compared against in tests.

## 6. The migration sequence

Each rung is independently shippable and each one keeps every existing test
green:

```text
1. CheckedProgram            (done: one coherent input; no behaviour change)
2. CoreTerm                  (done: explicit relational constructors)
3. CheckedQuery/CheckedExpr  (done: constructors enforce the rules)
4. erase()                   (done: checked IR -> existing Rel)
5. Rel -> existing lowerer   (done, unchanged)
6. Source elaboration        (done: `elaborate.rs` builds CheckedQuery from the
                              AST, reading types from the checker)
7. Production, parity-checked (done: `root_queries_checked` runs both paths and
                              fails closed when they disagree)
```

The remaining rungs, in order:

1. ~~**Restrict the escape hatches.**~~ **Done.** `root_queries_via_evaluator` is
   `pub(crate)`; `from_rel_unchecked` and `without_at` are `cfg(test)`. Narrowing
   them found out something at each step — see below.
2. ~~**Demote `schema` to an assertion.**~~ **Done, but not the way it was
   planned.** `schema` no longer *decides* anything: a malformed program is
   reported by the elaborated path as a `Fault::Program`. It still runs on the
   oracle path, because that path is consulted for definitions the routing layer
   is about to reject with a better message. See below.
3. **Delete the evaluator**, only once source-level and SQL golden coverage is
   broad enough to stand alone. Until then it is the oracle, and the parity
   check is what makes shipping both paths safe.

**Why rung 2 could not be done as written.** Replacing `schema_located` with
`debug_assert!` looked safe — with the checked tree emitted instead, all 40 frozen
outputs and all 400 tests were unchanged. That experiment had a blind spot: the
suite under the real demotion *panicked* on `q = table "s" "t"`, a table whose
defining definition declares no columns. `schema` had been producing that
diagnostic, and the elaborated path treated the same program as a capability gap,
so failing closed reported a compiler bug where the user deserved a plain error.
No example contains a column-less table, which is why the golden diff missed it.

So the real gap was that `core::Error` could not say *who was at fault*. It can
now: `Error::unsupported` marks a missing capability, and `root_queries_checked`
reports a `Program` fault with the elaborated path's own wording while a
`Compiler` fault fails closed. `schema`'s verdict is no longer final — that,
rather than deleting it, is what the demotion amounted to.

Note what rung 6 did *not* do: it did not make the evaluator consume
`CheckedQuery`. The original plan for this rung was to move the checked
constructors to the evaluator's call sites, and that turned out to be the wrong
shape — the evaluator is untyped at application and carries a `Value` model,
while `CheckedQuery` needs a row at every node. Elaborating from source beside
the evaluator, and comparing the two, reached the same goal with the oracle
intact. See the "Source elaboration now runs in production" section below.

Steps 1–3 are worth doing together: doing only step 1 leaves two
representations of the same query in the interpreter, which is worse than the
current state.

## Claims this step can support

* **One input.** A later phase reads `CheckedProgram`, not the AST plus
  `TypeCheck` plus `Workspace`. `CheckedDef` carries each definition's closed
  `ScalarType` and row, classified structurally by `TypeCheck::scheme_view`
  rather than by looking at a printed type, and `TypeCheck::choices_of` hands
  over the resolved overload choices under the key the checker actually used
  (the use expression's `ExprId`).
* **Bodies are opt-in and honest about it.** `CheckedProgram::of` does not
  evaluate, so `CheckedDef::terms` is `None` — "not elaborated" — which is a
  different statement from an empty list and is typed as one.
  `CheckedProgram::of_elaborated` fills the bodies in.
* **One place for relational validity per rule.** `where`, `select`, `update`,
  `agg`, `order`, join, set, and group rules are stated in the checked
  constructors and in `rules` — the same `rules` the checker's *types* use, so
  the phase and join-side rules have exactly one definition each (`JOIN_ONLY`,
  `needs_side`, `place`, `mix`, `nested`).
* **Almost** one place. Two honest exceptions, both listed above rather than
  papered over: `omit` re-states its rule instead of calling
  `schema::omit_columns`, and `schema` is still the *production* column
  validator on the evaluated path (`eval.rs` calls `schema_located`), not only a
  backend guard. It becomes a debug assertion only when the elaboration rung
  makes the checked path the production path.
* **Erase once.** `Rel` is produced at one boundary from a type-carrying tree;
  the SQL backend never sees a row type, an inference variable, or a phase.
* **Name-level validity is checkable; type-level validity is tested, not
  proved.** `schema::schema(&erase(q))` can only compare column names, so it
  pins the shape of the erasure and nothing about `ScalarType`. Type-level laws
  (outer-join nullability, `update` precedence) are pinned by row-level tests
  and by the SQL golden tests.
* **No flag day.** Everything above was added beside the existing pipeline, and
  the existing tests are the behavioural contract. Source elaboration now runs
  *in* production — `root_queries_checked` is the boundary the CLI and the LSP
  both go through, and it runs both paths and compares them — but it was added
  beside the evaluator rather than replacing it, so there was still no day on
  which the output could move.

## How this was verified

The behavioural contract for this step is the frozen CLI output, not the build:

```bash
nix develop --command cargo test --workspace            # 403 passed, 0 failed
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command cargo build --workspace -q
<regenerate examples/*.cagara: 6 dialects + --pretty + --types + stderr>
diff -r <frozen-before> <after>                        # must be empty
```

Two regressions during this work were invisible to `cargo build` (exit 0),
`cargo clippy` (clean), and `cargo test --no-run`:

1. an argument-order inversion in the primitive dispatch — 13 of 40 golden
   files wrong;
2. a half-landed set-operator declaration in `prelude.cagara` — all 40 files
   wrong, 12 CLI tests failing.

Only a real test run and the golden diff caught either. Treat those two as the
gates; treat the build and the linter as conveniences.

## The post-commit review, and what it changed

A review of the first commit found that the layer was **descriptive rather than
authoritative**. All seven findings were confirmed against the tree
(`verify/review-findings-verified.md`), and two were reproduced empirically
rather than argued:

* overload choices were keyed by `ExprId` in the checker and looked up by span
  offset here, so a real overloaded program transferred **zero** choices;
* `scheme_row` tested the *printed* scheme for the word `query`, so a function
  whose signature mentions a query was reported as a query with no columns.

Fixed since: the API boundary is enforced by the compiler; erasure returns
`Result` instead of manufacturing placeholder IR; the choices are read under
the right key; `terms` distinguishes "not elaborated" from "empty"; scheme
classification is structural; the unchecked bridge is named as one; and the
`Box::leak` in `first_plain_column` is gone. `clippy --all-targets -D warnings`
is now part of the gate.

**Source elaboration now runs in production, under a parity check.**

`root_queries_checked` — the one boundary the CLI and the LSP both go through —
runs *both* paths and compares them, and **fails closed**: if the trees differ,
or if source elaboration cannot build the definition at all, that definition is
reported as an internal error and **no relation is emitted for it**.

Failing closed is a temporary policy rather than the destination; the
destination is for source elaboration to be the only implementation, at which
point there is nothing to compare. Until then something must happen on a
disagreement, and the alternative — ship the evaluator's tree and warn — keeps
programs working but ships one of two trees the compiler has already concluded
it cannot trust. A compiler bug that produces no output is a bug report; one
that produces plausible SQL is a data incident. The cost is that a definition
affected by an elaboration bug stops compiling even though the evaluator could
have built it, which is why this is called temporary.

The one option ruled out is preferring the evaluator **quietly**: that is
precisely the defect this comparison exists to find. A production test
(`a_parity_gap_fails_closed_and_says_so`) pins the policy, including the
wording, so changing it is a deliberate act rather than a comment edit.

That check is live, not decorative. Routing it found two defects no test had:

* `q = q` overflowed the stack — descending into a definition means elaborating
  it, and the elaborator had no cycle guard where the evaluator refuses
  recursion (`eval.rs:89`). `Ctx` now carries the active definitions.
* A set operation reversed its operands. The two *spellings* are genuinely
  different desugarings: `_&~_ : ... = q => r => except q r` re-binds, so
  `users &~ admins` is `except users admins` (piped query left), while the bare
  name sits at `&`'s level, so `users & except admins` is `except admins users`
  (argument left). One operand order cannot serve both, and `union` being
  symmetric hid it until a `union` had differently-filtered operands.

**Resolved since the last revision:**

* **Point-free composition is now elaborated**, and how it was fixed is worth
  recording because the obstacle was structural rather than a missing case.
  `nextWeek = addDays 7 >>> truncWeek` desugars to
  `_>>>_ (addDays 7) truncWeek`, where `_>>>_` is `f => g => x => g (f x)` — an
  ordinary lambda over two *function* arguments — so `addDays 7` has to be held
  as a partially applied function and applied later.

  The earlier design kept three parallel binding environments (queries,
  expressions, stages) and decided what a name was by *which environment it came
  from*. A partially applied function belongs to none of them, so a fix along
  those lines needed `>>>`'s exact body spelled out and still failed. There is
  now one environment of [`CheckedValue`] — the counterpart of the evaluator's
  `Value`, which is a single `Env` for the same reason — and `apply_value`
  mirrors `eval::apply`: push an argument, then either complete the value or stay
  deferred. Nothing in that path looks at an operator's *name*.

  It is covered by five cases, including `<<<`, a three-stage chain, a composed
  function used twice, and the same composition written as a direct application
  of `_>>>_` — the last guarding against the operator being recognised by name.

**Still open:**

* `schema` still runs, on the oracle path, and still reports there. Its verdict is
  no longer final — the routing layer decides what the user sees, and a
  `Fault::Program` from the elaborated path wins — but it has not been removed,
  and on the oracle path it is still doing work the constructors already did.
* `omit` still restates a rule rather than calling `schema::omit_columns`.
* `from_rel_unchecked` still exists, now `cfg(test)`, and it is the only way to
  build a `CheckedQuery` without running the constructors. A shipping binary can
  no longer reach it, which was the property worth having; deleting it means
  deleting the bridge harness with it.
* The evaluator is still the oracle for every definition, so it cannot be
  deleted. Removing it means promoting the elaborator from "agrees with the
  evaluator" to "is the only implementation" — and the defects this section
  records are the argument for doing that one step at a time.

**What source elaboration now covers.** Overloads are resolved per *use*, not
per definition: a definition's open overloads are recorded as `Choice::Hole(k)`
where they are written, and the candidate is recorded against whichever
definition instantiates it. `h = e => users & select { x = e + 1, id = .id }`
therefore elaborates differently for `h .age` and `h 1.5`, and
`quad = x => twice (twice x)` resolves a nested hole from the assignment its
own use supplied. Lambda application binds parameters into **one** environment of
values — the counterpart of the evaluator's `Value` — so a parameter may be a
query, an expression, a callable, or a stage. An earlier revision kept three
parallel environments, one per kind, and decided what a name was by *which
environment it came from*; that could not represent a partially applied
function at all, which is why point-free composition (`f >>> g`) once failed. A
stage argument is recognised by being *partially* applied, since
`unionAll users users` supplies every argument and is a complete call.

**There are two differential harnesses, and only the second is evidence.**

The first (`assert_trees_agree`) compares the evaluator's erased `Rel` against
`Rel -> CheckedQuery -> erase`, over the real examples plus sixteen stage
combinations. It catches drift between the two *erasers*, and it found a real
defect in the bridge (`from_rel_unchecked` rebuilt an `omit`'s input row by
appending the omitted key, reordering the input's columns). But it does **not**
establish what the design needs: because the "checked" side is built from the
evaluator's own output, it cannot fail if source elaboration never constructs a
`CheckedQuery` at all — a vacuity, not coverage.

The second (`assert_source_elaboration_agrees`) is the source-level one:

```text
source -> type check -> checked elaboration -> erase      (elaborate.rs)
source -> existing evaluator -> CoreTerm -> erase_core    (oracle)
```

`elaborate.rs` walks each definition's AST and builds `CheckedQuery` /
`CheckedExpr` through the constructors, reading types from the checker rather
than re-deriving them (`use_ty` for a leaf, `scheme_fields` for a table's
columns, `choices_of` for a resolved overload, `result_expr` for the phase and
scalar a callee returns). An unsupported definition is a **failure**, not a
skip, and the harness separately asserts that every definition the evaluator
produced a relation for was *considered* — so coverage cannot quietly shrink.

Coverage, measured rather than assumed: **11 of 11** query definitions across
`report.cagara` (9), `public.cagara` (1) and `schema.cagara` (1), plus eighteen
stage combinations and ten cases built from constructs the examples do not
contain (`inList`, a three-way join, an aggregate followed by a projection).
Mutation-checked: perturbing the `Omit` eraser fails it with
`omit: q elaborates from source to a different tree than the evaluator
produces`.

The same comparison also runs **in production** now, on every compile, inside
`root_queries_checked` — so agreement is asserted where it matters rather than
only in tests. The evaluator stays as the behavioural oracle, and on disagreement
the compiler **fails closed**: it reports an internal error and emits no relation
for that definition, rather than shipping either tree. See the policy note on
`root_queries_checked`; briefly, a disagreement means the compiler has concluded
it cannot trust either tree, and a compiler bug that produces no output is a bug
report while one that produces plausible SQL is a data incident.
