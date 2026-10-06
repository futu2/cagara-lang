# Review findings: verified against the tree (lead)

Response to the post-commit review of `bdc05cb`. Every finding was checked in
the source and, where a claim was empirical, reproduced with a probe on the
real `Workspace`/`CheckedProgram` API rather than argued from the code alone.
Verdicts: **CONFIRMED** / **PARTLY** / **REFUTED**.

| # | Finding | Verdict |
|---|---|---|
| 1 | checked invariant not enforced by the API | **CONFIRMED** |
| 2 | production path does not use the checked layer | **CONFIRMED** |
| 3 | `CheckedProgram::of` passes `HashMap::new()` for bodies | **CONFIRMED** |
| 4 | overload choices copied under the wrong key | **CONFIRMED (empirically)** |
| 5 | erasure silently fabricates invalid IR | **CONFIRMED** |
| 6 | scheme extraction uses text heuristics | **CONFIRMED (empirically)** |
| 7 | `first_plain_column` leaks per call | **CONFIRMED** |

The review's framing is accepted: the checked layer is currently *descriptive,
not authoritative*. That is a fair reading of what shipped, and the fix order it
proposes (make `CheckedProgram` real and invalid values unconstructible before
touching SQL lowering) is the right one.

---

## 1. CONFIRMED — the API does not enforce the invariant

`CheckedQuery` (`checked.rs:306`), `CheckedExpr` (`:806`), `CheckedQueryNode`
(`:691`) and `CheckedExprNode` all have public fields, so any caller can build a
`CheckedQuery { row, node, origin }` directly and bypass the constructors that
are the entire source of the layer's guarantees. `docs/CHECKED-CORE.md` states
"the constructors are the only way to build one"; Rust does not enforce that,
so the document is making a claim about discipline, not about types.

`from_rel` is the second door: it rebuilds a `CheckedQuery` from an unchecked
`Rel` and has to be told what it is looking at (`StageHint`) precisely because a
`Rel` under-determines the answer.

## 2. CONFIRMED — the checked layer is not on the production path

`eval.rs:515 root_queries_checked` still constructs an `Evaluator`, erases a
`CoreTerm`, and calls `schema::schema_located`. `grep` for `CheckedProgram` or
`CheckedQuery` outside `checked.rs` finds only doc-comment mentions. So the
checked layer has zero callers in `cargo-hir`, `cagara-cli`, or `cagara-lsp`,
and the SQL golden outputs are produced without it. Confirmed independently by
`verify/report-final.md` (the verifier's structural finding) before this review.

## 3. CONFIRMED — `CheckedDef.terms` is always empty in practice

`CheckedProgram::of` (`checked.rs:148`) calls
`from_type_check(ws, &tc, HashMap::new())`. Nothing else populates `terms`, so
every `CheckedProgram` produced by the documented entry point has
`CheckedDef.terms == []` while the field's doc comment claims "one core body per
overload-hole instantiation". Reproduced: `terms len: 0` for a real query.

## 4. CONFIRMED, empirically — choices are keyed by `ExprId`, looked up by span

The decisive evidence, which the finding identifies correctly:

* `check/infer.rs:953` — `self.lookup(n, e.id, sp)`: the `site` argument is the
  **`ExprId`** of the name expression (`ast.rs:83`, `pub id: ExprId`), not a
  byte offset. `def_type`/`overload_type` thread it into `Origin::Site`, and
  `overload.rs:216 record()` stores `choices[(module, def)][(site, k)]`.
* `checked.rs:170` iterates `body_spans(&d.body)` and probes
  `tc.choice(index, def, span.start, k)` / `span.end` — **span offsets**.

Reproduced with a genuine user overload:

```cagara
users : query { id = int, age = int, active = bool } = table "p" "users"
describe : expr r int  -> expr r string = sql "CAST($1 AS TEXT)"
describe : expr r bool -> expr r string = sql "CASE WHEN $1 THEN 'yes' ELSE 'no' END"
q = users & select { a = describe .age, b = describe .active }
```

```
checker choice site=9  -> Def(1, 1)      (ExprId 9)
checker choice site=12 -> Def(1, 2)      (ExprId 12)
CheckedProgram choices for q: 0          <- nothing transferred
```

So the transfer fails for exactly the programs it exists for. The existing test
inserts a choice by hand, which is why it passes; it never exercises extraction.
Span and `ExprId` are different namespaces that happen to be small integers, so
a lookup can also *collide* and silently transfer the wrong choice — worse than
transferring none.

## 5. CONFIRMED — erasure invents nodes instead of failing

* `core_term.rs:814~` `erase_core`'s catch-all maps a non-query `CoreTerm` to
  `Rel::Table { schema: "", name: "/* not a query: … */" }` — a **fake table**.
* `checked.rs:1374~` `erase_expr` maps `CheckedExprNode::Call` to
  `Expr::Tpl("/* unresolved call `name` */")` — a **fake SQL template**.

Both are documented as internal errors, and both then flow into the SQL
backend as ordinary IR. A `Result` (or separate query/expression types) would
turn "this cannot happen" into a compiler-checked fact. As written, a bug
becomes silently wrong SQL instead of a diagnostic.

## 6. CONFIRMED, empirically — text heuristics misclassify schemes

`checked.rs:250 scheme_row` decides "is this a row?" with
`printed.contains("query")`. Reproduced:

```
 users  printed=query { id = int }                     row=Some(["id"])
     f  printed=query { id = a | b } -> query { id = a | b }  row=Some([])   <- MISCLASSIFIED
     g  printed=query { id = int }                     row=Some(["id"])
```

`f = q => q & where (.id > 0)` is a **function**; `contains("query")` is true for
its argument *and* its result, so it is reported as a query with **zero columns**
rather than as a function. Anything whose signature mentions `query` lands here,
including helpers, join builders, and set-operation wrappers.

Separately `TypeCheck::def_scalar` (`check/mod.rs:172`) documents "`None` when
the scheme has no scalar reading (a function or a row)" but is
`Some(ty_to_scalar(&s.ty))` unconditionally — it returned `Some(Unknown)` for
`users`, `f`, and `g` alike. The doc describes behaviour the code does not have.

The root cause is shared: `ty_fields`/`ty_to_scalar` return `vec![]`/`Unknown`
for *every* non-row/non-scalar type, so neither the empty row nor `Unknown` can
be distinguished from "not applicable". A structural view is needed (the
review's `SchemeView::{Query(RowType), Scalar(ScalarType), Function(..), Open}`
is the right shape), not a better string test.

## 7. CONFIRMED — `first_plain_column` leaks

`core_term.rs:650`: `found.map(|n| Box::leak(n.into_boxed_str()) as &str)`.
Every call leaks one small allocation permanently. Called from
`CoreTerm::join`'s bare-`.x` check, so a program with many joins leaks once per
check. The borrow could be returned directly if `visit_columns` were
restructured to stop at the first match, or an owned `String` returned.

---

## What this changes about the previous report

The claims in `docs/CHECKED-CORE.md` that need to weaken further, beyond the
name-level caveat already recorded:

* "A later phase reads `CheckedProgram`" — true of the type, false of the
  production path (finding 2), and the one phase that could read it gets empty
  `terms` and no choices (findings 3, 4).
* "the constructors are the only way to build one" — a doc claim, not an
  enforced one (finding 1).
* "Erasure is total and structural" — total only because impossible inputs are
  given fabricated nodes rather than errors (finding 5).

None of this invalidates the layer's rules: the constructors, the row laws, and
the phase/join-side reuse of `rules` were verified independently and hold. The
problem is that nothing forces anyone to go through them.
