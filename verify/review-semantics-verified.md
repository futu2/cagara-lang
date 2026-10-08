# Review findings: language & semantics, verified against the tree

Response to the second review (cycle detection, `agg` + `where` → `HAVING`, a
`qualify` stage, and a runtime-parameter guardrail). Every finding was checked
in the source and, where the claim was empirical, reproduced with the real
`cagara` binary or the `Workspace`/`root_queries` API rather than argued from
the code alone. Verdicts: **CONFIRMED** / **PARTLY** / **REFUTED**.

| # | Finding | Verdict | Action |
|---|---|---|---|
| 1 | a recursive helper surfaces as an internal / compiler-budget error | **REFUTED** | regression test added for the untested forms |
| 2 | a filter over aggregate output becomes an outer `SELECT`; fold it into `HAVING` | **CONFIRMED** | implemented in `cagara-sql` |
| 3 | `qualify` would remove the derived table a window filter needs | **CONFIRMED** (already roadmap) | left deferred; payoff recorded |
| 4 | the runtime-parameter roadmap item has no anti-injection guardrail | **PARTLY** | guardrail stated in `docs/PLAN.md` |

---

## 1. REFUTED — recursion is already a named diagnostic

`Ctx.active` (`elaborate.rs:89`) is the stack of definitions currently being
expanded, and `enter` (`elaborate.rs:97`) refuses to re-enter one:

```rust
if cx.active.contains(&(module, def)) {
    return Err(Error::new(format!(
        "`{name}` refers to itself; recursion is not supported"
    ))
    .at(Origin::new(module, definition.span)));
}
```

It is not a budget or internal error, and it fires for all three shapes:

```text
/tmp/rec1.cagara:2:1: error: `f` refers to itself; recursion is not supported
 2 | f = x => f x
```
```text
/tmp/rec2.cagara:2:1: error: `f` refers to itself; recursion is not supported
 2 | f = x => g x          # mutual recursion, reported at the first cycle member
```
```text
/tmp/rec3.cagara:2:1: error: `q` refers to itself; recursion is not supported
 2 | q = q & where (.id > 0)
```

What *was* true is narrower than the finding claims: the only test of this
behaviour covered the query-level self-reference (`tests.rs:524`,
`q : query { id = int } = q`). The helper-lambda and mutual forms the review
names were untested, so the test now pins them
(`crates/cagara-sql/src/tests.rs`, `errors()`); note the failure is reported at
the *use*, because a helper has no compiled query of its own.

## 2. CONFIRMED — a filter over an aggregate is now `HAVING`

Reproduced before the change with `examples/report.cagara`:

```sql
-- revenue (before)
SELECT user_id, revenue, n FROM (SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n
FROM public.orders WHERE (status = 'paid') GROUP BY user_id) AS t1
WHERE (n >= 5) ORDER BY COALESCE(revenue, 0.0) DESC NULLS LAST;
```

and after:

```sql
-- revenue (after)
SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n FROM public.orders
WHERE (status = 'paid') GROUP BY user_id HAVING (COUNT(*) >= 5)
ORDER BY COALESCE(SUM(amount), 0.0) DESC NULLS LAST;
```

The review's premise needs one correction before it is implementable: "the
predicate only references aggregate outputs" is automatic after `agg`, but the
output *name* is not usable in `HAVING` — PostgreSQL rejects `HAVING n >= 5`.
`Stage::item` already resolves an output name to the expression behind it, so
the fold inlines the aggregate expression instead of hoping an alias works.

The "no intervening `order`/`limit`" condition is structural rather than
tracked: the peel loop takes `base` to be the relation under the whole run of
`where`/`at`, so `base` is `Rel::Agg` only when nothing sits between it and the
filter. `agg & order & where` and `agg & limit & where` leave `Rel::Order` /
`Rel::Limit` as `base` and keep the derived table, which is required — the
filter must run after the sort/page. See `lower.rs:296-315`.

Coverage:

* `filter_after_agg_becomes_having` — no derived table, `HAVING (COUNT(*) >= 5)`.
* `consecutive_aggregate_filters_are_conjoined_having` — two filters AND together.
* `filter_after_agg_stays_outside_order_and_limit` — the two wrapper cases.
* `optimizer_keeps_stage_boundaries` — `--optimize` keeps the filter in `HAVING`.
* `engines.rs` `hav` / `havconst` — the differential harness runs the same
  query on SQLite and DuckDB, so the fold is checked against real engines, not
  only against emitted text.

## 3. CONFIRMED, already roadmap — `qualify`

The roadmap already listed `qualify` (`docs/PLAN.md`), so this is not a new
finding. The payoff is real and still unclaimed: the window filter in
`examples/report.cagara` lowers to a derived table today.

```sql
-- latest
SELECT id, user_id, rn FROM (SELECT id, user_id,
ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY created_at DESC NULLS LAST) AS rn
FROM public.orders) AS t1 WHERE (rn <= 3);
```

Left deferred. The roadmap bullet now names the concrete motivation instead of
just the keyword.

## 4. PARTLY — the guardrail was missing, not the item

The roadmap did list "typed runtime parameters and prepared-query metadata"
(`docs/PLAN.md:65`), but said nothing about how a parameter reaches SQL, so the
review's concern is a real omission. The bullet now states that a parameter
compiles to a bind placeholder, never to statement text and never to an
identifier, and that the test must assert no parameter value appears anywhere in
the emitted SQL. No compiler code changed: the feature is not implemented yet.

---

## Also fixed while here

`docs/LEARN.md` used `total = sum .amount` followed by `where (.total > 100.0)`
as its derived-table example. `sum` is nullable, so that program is a type
error in the current tree (`only one side is nullable`). The rewritten section
uses `coalesce 0.0 (sum .amount)`, and the surrounding text now describes
`HAVING` instead of an outer query, matching what the compiler emits.

## Verification

* `cargo test --workspace` — all tests pass.
* `cargo fmt --check` — clean.
* `CAGARA_REQUIRE_ENGINES` is not set locally and `duckdb` is not installed, so
  the differential suite ran only SQLite here; CI runs both engines.
