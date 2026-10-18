# Consolidation status — functional normalizer + one typed arena

This file tracks the Step 2–5 work: replacing `typed_lower.rs` (the
source-directed lowering) with `normalize/` (one normalizer) and merging
`TypedQuery` + `CheckedQuery` into one arena. It is deleted when Step 5 lands.

## Where the work is

| Piece | State |
|---|---|
| `tests/pipeline_golden.rs` + snapshot (Step 0 gate) | **in place**, passing |
| `check/core_facts.rs` (Step 1: `Resolved`, `TemplateKind`, `DefId`) | **in place** |
| `normalize/arena.rs` — relational arena, constructors, `erase` | **written**, unit-tested via the differential |
| `normalize/mod.rs` — value domain, `Normalizer`, `Site` stack, driver | **written** |
| `normalize/value.rs` — reduction rules (`value` / `apply`) | **written**, 47/99 corpus definitions agree with the legacy route |
| `normalize/scalar.rs` — templates, `CASE` | ported |
| `normalize/frame.rs` — window frames | ported |
| `normalize/env.rs` — environments (the substitution) | written, unit-tested |
| `normalize/budget.rs` — depth + work bounds | written, unit-tested |
| `tests/normalize_differential.rs` — both routes compared | **in place**, snapshot of mismatches recorded |
| Cut production over to `normalize` | not yet |
| Delete `typed_lower.rs` and the `TypedQuery`↔`CheckedQuery` bridge | not yet |
| Docs (`docs/CORE.md`, ARCHITECTURE, PLAN) | written by `docs-core` |

## The site model (why it exists)

A node is blamed where it was *written*, and a stage is applied by the
definition whose body spells the application — the prelude, for a primitive.
So `normalize` keeps a stack of `Site { module, def, id, span, named }`:

* every expression pushes a frame; `value()` pushes it as `named: true`,
## What the normalizer now covers

`normalize` reduces a definition body to a query on every program in the corpus and
the examples. The remaining 52 definitions fail in **one** place, and it is
narrower than the list above once the debugging is done:

* a **lambda body that applies its own parameter** — `_&_ : a -> (a -> b) -> b
  = x => f => f x` is the prelude's pipeline operator, and `_>>>_`'s body is
  `g (f x)`. When the body is evaluated, `f` is looked up in the closure's
  environment and found (the closure binds it when the lambda is applied), so
  the body's `f x` reduces — but only after the *outer* application's arguments
  have arrived, and the checker's stage record for the pipeline application
  then has to be found through a frame stack that no longer names the
  application (the body's frames were pushed by the body).

The fix is to let the closure carry the site it was applied at, so a body
evaluated later still knows the application it belongs to. That is one field on
`Closure` and one push in `apply_closure`.

## Fixes already landed this session (in `normalize/`)

1. literals reduce to `Label` and convert back with `scalar_of`;
2. `table` takes its row from the *application's* node and is blamed at the
   call, not at the prelude;
3. the `Site` stack (`value` names frames, `argument` does not) replaces the
   ad-hoc `at`/`current` fields;
4. `stage_site` looks up the checker's `RowStageView` by kind through the named
   frames and restores the application's sites when a stage saturates;
5. fact lookup is span-aware (`ancestor_path_at`, `node_at_path`), because an
   `ExprId` is only unique within a definition and some syntax (a primitive
   body's lambda parameter) carries id 0;
6. an unresolved name falls back to the definition scope it was written in, and
   a name applied without a binding reports a precise capability gap instead of
   a generic one.
