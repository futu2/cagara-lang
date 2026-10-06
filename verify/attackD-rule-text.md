# Attack D: rule-text duplication (verifier)

Companion to `attackD-row-laws.md` (lead, row formers). This file covers the
*message* half of attack D: every rule whose text appears at more than one
site, and whether the copies agree.

Method: `grep -rn '<message fragment>' --include=*.rs crates/` for each rule,
then confirmed the live wording with `./target/debug/cagara <file>`.

## D1 — CONFIRMED: duplicate-field rule, five wordings, and `update`'s is not reproduced

`checked.rs:1139-1168` `project_row` is the shared body of `select`, `update`
**and** `agg` (called at `checked.rs:314`, `:332`, `:352`). Its duplicate check:

```rust
// checked.rs:1163-1166
let row = RowType::project(&out);
if let Some(dup) = row.duplicate() {
    return Err(Error::new(format!("`{stage}` has two fields named `{dup}`")).at(origin));
}
```

`RowType::duplicate` (`core.rs:204-213`) returns only the repeated *name*; it
cannot tell `select` from `update` from `agg`.

| site | message |
|---|---|
| `check/infer.rs:1914` (`Cons::Update`, dedicated `seen` loop at :1909-1916) | `` field `n` appears twice in `update` `` |
| `check/infer.rs:1169` (record literal) | `` field `k` appears twice `` |
| `check/infer.rs:599`, `:665` | `` field `k` appears twice in a record type `` |
| `eval.rs:243` | `` field `k` appears twice `` |
| `checked.rs:1165` | `` `{stage}` has two fields named `{dup}` `` |

Live wording (the checker's, from the record path — not the `update`-specific one):

```
$ ./target/debug/cagara /tmp/dup.cagara
/tmp/dup.cagara:2:16: error: field `id` appears twice
 2 | u = t & update { id = 1, id = 2 }

$ ./target/debug/cagara /tmp/dup2.cagara
/tmp/dup2.cagara:2:16: error: field `id` appears twice
 2 | u = t & select { id = .id, id = .n }
```

So `core.rs:259-261`'s claim — "`message` is the diagnostic the schema layer
would have produced for the same program; the constructors keep the wording so a
user sees one explanation, not two" — does not hold for this rule. The checked
layer adds a third explanation and drops the checker's stricter
`Cons::Update`-specific check (which runs on the *field list*, before merging
over the input, whereas `project_row` checks the *projected output row*).

`checked.rs:1161-1162` asserts "The checker already rejects it as a record with a
repeated field" — true, but with different text, and not via the `update` rule.

## D2 — `update` needs-at-least-one-field: three sites, text agrees

| site | message |
|---|---|
| `check/infer.rs:1909` | `` `update` needs at least one field `` |
| `schema.rs:215` (`merge_columns`) | `` `update` needs at least one field `` |
| `checked.rs:1149` (via `project_row("update", ..)`) | `` `{stage}` needs at least one field `` |

Text matches. Latent divergence only: `checked.rs` cannot route through
`schema::merge_columns` so it re-implements the rule inside the shared
`select`/`agg` path.

Also present: `check/infer.rs:1801` and `schema.rs:195` both emit
`` `{stage}` needs at least one field `` for select/agg — consistent.

## D3 — CONFIRMED (this is the attack-C counterexample): `omit` missing key

| site | behaviour |
|---|---|
| `schema.rs:243-248` (`omit_columns`) | `Err("no column `{key}`; available: {cols}")` |
| `checked.rs:213-224` (`RowType::omit`, core) | total, no-op on missing key |
| `checked.rs:439-450` (`CheckedQuery::omit`) | **no check at all**, always `Ok` |

Reachable with only the `pub` API:

```rust
let t = CheckedQuery::table("s", "t",
    Some(RowType::new(vec![("id".into(), ScalarType::Int)])), o)?;
let q = CheckedQuery::omit(t, "nope", o)?;  // Ok
q.erased_schema()  // Err("no column `nope`; available: id")
```

`core.rs:207-214` justifies totality with "the checker is what rejects a
missing key". For a layer whose contract is "a `CheckedQuery` that exists has
already been checked", the reasoning is backwards: the constructor is the
place the rejection must happen.

## D4 — `JOIN_ONLY` / `needs_side`: SINGLE SOURCE, no drift (positive result)

`rules::JOIN_ONLY` (`rules.rs:8`) is used verbatim at:
`schema.rs:279`, `check/infer.rs:1753`, `:1816`, `:1930`, `checked.rs:1128`.

`rules::needs_side` (`rules.rs:46`) at:
`schema.rs:141`, `check/infer.rs:1866`, `checked.rs:513`.

No copy has different wording or a weaker condition. This is exactly the
property the refactor is for; recording it as a pass.

**Known non-convergence (pre-existing, not introduced here):**
`cagara-sql/src/lower.rs:594` produces a *fourth* spelling of the side rule,
`"join predicates need `.<{n}` or `.>{n}`"`, for the same situation. Outside
the checked layer's scope; should not block, but it is not "one message".

## D5 — `where` / window / agg placement: single source (positive result)

Every phase-placement message in `rules.rs:65-91` (`place`) is produced in one
place. `checked.rs:1101-1109` `place()` wraps `rules::place`, and
`check/infer.rs`'s `stage_phase`/`place` (`infer.rs:2228-2262`) call the same
`rules::place`. No duplicated phase text found.

Note `checked.rs:292` and `:504` add their **own** bool checks
(`"a `where` predicate must be bool, found {}"`, `"a join predicate must be bool,
found {}"`) rather than deriving them from the checker; `check/infer.rs:1870`
has `"a join predicate must be bool, found {}"` — that one **does** agree
verbatim. The `where` bool text has no checker counterpart because the checker
enforces it by unification, so there is nothing to compare against.
