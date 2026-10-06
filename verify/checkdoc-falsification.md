# Falsification pass: `docs/CHECKED-CORE.md` (verifier)

Every claim in the migration note, checked against the tree as it now stands.
Verdicts: **HOLDS** / **OUTDATED** (was true, the tree moved) / **FALSIFIED** /
**IMPRECISE** (true in spirit, wrong in detail).

I authored none of this document and did not edit it. Line cites are to
`docs/CHECKED-CORE.md` unless stated.

---

## C1 — "Erasure is total and structural" (`:153`)
**HOLDS.** `checked.rs:1264-1339` `erase` is a total `match` over
`CheckedQueryNode`, one arm per variant, no `unwrap`/`expect`/indexing
(`grep -n '\.unwrap()\|\.expect(\|panic!\|\[0\]' crates/cagara-hir/src/checked.rs`
→ no hits in `erase`). `erase_core` (`core_term.rs:750-821`) is likewise total
and has an explicit catch-all at `:809-813` for a non-query term.

## C2 — "a `CheckedQuery` that exists is already valid" (`:155`)
**WAS FALSIFIED, NOW HOLDS (name-level only).**
- Falsified as written: `CheckedQuery::omit(input, missing_key)` returned `Ok`
  while `schema::schema(&erase(q))` returned `Err` — reachable through `pub`
  API alone. Reported as my attack-C counterexample; now fixed
  (`checked.rs` `omit` checks `input.row.has(&key)` first, message matching
  `schema.rs:245`).
- **Still IMPRECISE**, and this matters: "valid" is only established at the
  **name** level. I injected the original `update` bug (`overwrite` → `merge`)
  back into `checked.rs` and my 22-case adversarial probe **passed**.
  `schema::schema` returns `Vec<String>` (`schema.rs:8`), so `erase`/`schema`
  agreement cannot see a wrong column *type*. See
  `verify/attackE-nonvacuity.md`.

## C3 — "`CheckedQuery::erase()` and `CoreTerm::erase()` agree by construction" (`:157`)
**FALSIFIED (naming).** There is no `CoreTerm::erase()`. The functions are
`erase_core` (`core_term.rs:750`) and, on the checked side, an inherent
`CheckedQuery::erase` (`checked.rs:662`) plus the free `erase`
(`checked.rs:1264`). The *claim* (agreement) is plausible and separately
evidenced: both stamp `Rel::At` and both suppress double-wrapping
(`checked.rs:1271-1274`, `core_term.rs:256-265`), and `CoreTerm::of_checked`
(`core_term.rs:877-978`) stamps the same location from `q.origin`.
**Fix the sentence, not the code.**

## C4 — "$schema::schema(\&erase(q)) == q.row.columns()$ is checked in tests" (`:163-168`)
**HOLDS, but weaker than it reads.** Present at
`checked/tests.rs:739-775` (`schema_of_the_erasure_equals_the_recorded_row` and
`..._for_every_small_stage`). Note the tests compare **names**, since
`schema::schema` returns names; the doc's `q.row.columns()` is
`Vec<(String, ScalarType)>`, so the equality as literally written does not
compile. The real assertion is on names. **IMPRECISE.**

## C5 — "`CoreTerm` contains no closures, no environments, and no partial applications" (`:105-108`)
**HOLDS.** `grep` for `Closure|Env|Rc<|partial` in the `CoreTerm` definition
(`core_term.rs:48+`) → no hits. (The *evaluator* still has them; `CoreTerm`
does not.)

## C6 — The constructor rule table (`:126-137`)
**HOLDS for the rules, with one wording defect.**
- `where`: row-phase bool + output row = input row ✅ (`checked.rs` `where_`)
- `select`: row-phase, input columns only, output row = field list ✅
- `agg`: agg/const phase ✅
- `order`: row-phase keys ✅
- `join`: row-phase `on`; `.<x`/`.>x` required, bare `.x` rejected via
  `rules::needs_side`; outer side `maybe` ✅ (verified against the `--types`
  oracle for all six kinds — attack A)
- `set`: "both inputs expose the same row" ✅ — and in fact **stricter** than
  `schema`, which compares names only while `CheckedQuery::set` compares types
  (`checked.rs:544-565`).
- **`update`: "output row = fields merged over the input row" is AMBIGUOUS in
  the doc, but the *code* is now explicit and correct.** `checked.rs:390-396`
  documents it as `RowType::overwrite` (right-wins), *not* `RowType::merge`
  (left-wins), and explains the failure mode. `checked.rs:403` uses
  `overwrite`. The document's word "merged" should be "overwritten" to avoid
  re-introducing the confusion that `core.rs:256-268` now documents at length.

## C7 — "Phase and join-side rules come from `crate::rules` ... one statement of the rule, not a third one" (`:139-141`)
**HOLDS (verified as a positive result).** `rules::JOIN_ONLY` has one
definition (`rules.rs:8`) and five call sites (`schema.rs:279`,
`infer.rs:1753,1816,1930`, `checked.rs:1128`); `rules::needs_side` one
definition and three sites. No duplicated wording, no drifted condition. See
`verify/attackD-rule-text.md` §D4. **Caveat:** `cagara-sql/src/lower.rs:594`
still emits a fourth spelling of the side rule for the same situation —
pre-existing, outside this layer.

## C8 — "Column-level rules come from `crate::schema`" (`:142-143`)
**PARTLY FALSIFIED (one instance remains).**
- **Fixed since I first reported it:** the duplicate-field rule now reads
  `` field `{n}` appears twice in `{stage}` `` (`checked.rs:1234`) with the
  checker's per-stage wording cited in a comment (`:1229-1231`), pinned by
  `checked/tests.rs:681-693` (``appears twice in `select` `` /
  ``appears twice in `update` ``). My attack-D §D1 finding is **retired**.
- **Still true:** `RowType::rename_all` reuses `schema::rename_columns`
  (`core.rs:229`) ✅, but `omit` does **not** reuse `schema::omit_columns` —
  `checked.rs:509+` re-implements the check with duplicated message text.
  Behaviourally correct and the wording matches `schema.rs:245`
  (`omit_and_schema_agree_on_a_missing_key`), but it is a second copy of the
  rule, which is what the sentence denies.
- `core.rs:259-261`'s "the constructors keep the wording so a user sees one
  explanation, not two" was FALSIFIED when I reported it (a fifth wording in
  `project_row`); the per-stage fix above restores it for the duplicate-field
  rule. Re-check remaining message sites before re-asserting it generally.

## C9 — "SQL lowering is not redesigned" / "The CLI and LSP are unchanged" (`:210-214`)
**HOLDS.** `git diff --stat HEAD -- crates/cagara-sql/ crates/cagara-cli/
crates/cagara-lsp/` → empty. The frozen 40-file baseline diff is EMPTY,
including `examples/errors.cagara`'s `.err`, which is the behavioural contract
for exit codes and diagnostics.

## C10 — "`schema` remains only as a backend guard and as a test oracle" (`:222-224`)
**HOLDS, and is now load-bearing in a way the sentence understates.**
Production uses: `cagara-sql/src/lib.rs:40` and `lower.rs:708` (the backend
guard), and **`eval.rs:531` `schema_located(&rel)`**, i.e. the *evaluator's
only* column validation. So `schema` is not merely a backstop; it is the
production validator on the evaluated path. The doc should say so.

## C11 — "No flag day. Everything above was added beside the existing pipeline" (`:229-231`)
**HOLDS.** The checked layer still has zero production callers: the pipeline is
`eval::root_queries_checked` → `Evaluator` → `schema_located` (`eval.rs:488-531`),
and `grep` for `CheckedQuery|CheckedProgram|...` outside `checked.rs` finds only
doc-comment mentions. The rung-6 work (migration step 6, `:243`) is genuinely
not started, exactly as the doc says.

---

## Summary

| # | claim | verdict |
|---|---|---|
| C1 | erase is total | HOLDS |
| C2 | exists ⇒ valid | was FALSIFIED (omit); now HOLDS at name level only |
| C3 | `CheckedQuery::erase()`/`CoreTerm::erase()` agree | FALSIFIED (no such fn) — claim itself plausible |
| C4 | `schema(&erase(q)) == q.row.columns()` in tests | HOLDS but compares names; as written it does not typecheck |
| C5 | CoreTerm has no closures/envs | HOLDS |
| C6 | constructor rule table | HOLDS; doc wording "merged" should be "overwritten" |
| C7 | rules stated once | HOLDS (positive result) |
| C8 | column rules come from `schema` | PARTLY FALSIFIED (omit re-implements; duplicate-field wording now fixed) |
| C9 | SQL/CLI/LSP unchanged | HOLDS |
| C10 | `schema` is backend guard + test oracle | HOLDS, understated — it is the evaluator's validator |
| C11 | no flag day | HOLDS |

**The single most important correction:** C2. The document's "a `CheckedQuery`
that exists is already valid" is the load-bearing claim of the whole design, and
it is true only for *column names*. Wrong column *types*, wrong `maybe`
wrapping on an outer join, and wrong `update` precedence are all outside what
`schema::schema` can see, and my injection experiment proves the invariant test
does not catch them.
