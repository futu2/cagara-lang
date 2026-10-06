# Gate run log (verifier)

Frozen diff and gate runs, in order. `cargo build` exit 0 is NOT evidence of
correctness — the frozen diff is the primary gate (learned the hard way: see
the regression entry below).

| when | command | result |
|---|---|---|
| start | `diff -r /tmp/cagara-baseline-frozen /tmp/cagara-after` | **EMPTY** (40 files) |
| after core_term.rs landed | same | **EMPTY** |
| after elaborator's eval.rs migration | same | **13 files DIFFER** — REGRESSION |
| after `prims.rs` arg-order fix | same | **EMPTY** (40 files) |

## Regression (cleared): `prims.rs` curried-argument inversion
`cargo build -q` exit 0 AND `cargo test --no-run` green, yet:

```
$ ./target/debug/cagara /tmp/min.cagara
t : query { id = int, name = string } = table "public" "users"
q = t & select { id = .id, dn = .name }
```
```
-- t
SELECT id, name FROM public.users;
<prelude>:37:38: error: expected a query, found a record
 37 | _&_ : a -> (a -> b) -> b = x => f => f x
```

Root cause: `prims::call` receives a `Vec<Value>` and saturates on `arity()`,
so a permutation type-checks. The prelude signatures are value-first
(`prelude.cagara:60` `select : expr r (row s) -> query r -> query s`), and the
original `prims::call` read them in that order. The rewrite read the query
first: `CoreTerm::select(core_query(next())?, core_fields(next())?)` — Rust
evaluates arguments left-to-right, so `core_query` consumed the *record*.

**9 arms inverted**, verified against `git show HEAD:crates/cagara-hir/src/prims.rs`
and every prelude signature:
`Select, Update, Omit, Prefix, Suffix, AggStage, Order, Limit, Offset`.
`Where` was already correct (local `pred` bound first); `Join`/`Set` were never
broken. `examples/public.cagara` and `examples/report.cagara` lost 13 of 40
baseline files.

Fixed at `prims.rs:303-330` (stage argument bound to a local before the
`next()` that reads the query).

## In-flight gate run — 3 failures captured (tree moved mid-edit)
Caught between compiles; `cagara-hir` test binary **SIGABRT**:

```
test core_term::tests::table_columns_mut_descends_through_at_for_attach_schema ... FAILED
test eval::elaboration_tests::set_stage_takes_left_then_right ... FAILED
test eval::elaboration_tests::where_order_limit_elaborate_to_distinct_constructors ... FAILED
fatal runtime error: stack overflow, aborting
process didn't exit successfully: ... (signal: 6, SIGABRT)
```
Other binaries in the same run: `cagara-syntax` 51 passed/0 failed,
`cagara_fmt` 16 passed/0 failed, `cli.rs` 21 passed/0 failed.
`cagara_hir` did NOT complete, so this is **not** a green gate.

Two of the three failures are in the elaborator's brand-new
`eval::elaboration_tests` — the very tests added to pin argument order. Note
`set_stage_takes_left_then_right` FAILING is consistent with my warning that a
`Set` test asserting only `is_ok()` cannot detect a left/right swap, but here it
appears to be failing the other way (asserting order and finding it wrong), which
needs the current tree to confirm.

## Stack overflow: guard is CORRECT, cause is frame size
`MAX_DEPTH` (`eval.rs:43` = 256) guard **is reached and fires**:

```
$ cat /tmp/mutual.cagara        $ ./target/debug/cagara /tmp/mutual.cagara
f = x => g x                    /tmp/mutual.cagara:1:10: error: `f` evaluates
g = x => f x                    more than 256 calls deep; recursion is not
q = f 1                         supported
```
So the "guard was bypassed by the `Prim`/`Tpl` saturation arms" hypothesis is
**FALSIFIED**: `Value::Prim` without saturation returns `Ok(Value::Prim(..))`
without recursing, and `build_tpl`/`prims::call` run post-saturation.

Cause is per-frame stack cost: lead measured `size_of::<Value>()` 80 → 136
(`CoreTerm` 128, `Rel` unchanged 80). 256 frames × several `Value`s each × 56
extra bytes overflows the 2 MiB test thread but not the CLI's 8 MiB main
thread — which is exactly the CLI-fine / test-SIGABRT split.

## Final gate — GREEN (independently run by verifier)
```
cargo test --workspace
  cli      21 passed / 0 failed
  cagara_fmt 16 passed / 0 failed
  cagara_hir 198 -> 204 passed / 0 failed
  lsp      20 passed / 0 failed
  cagara_sql 51 passed / 0 failed
  engines   2 passed / 0 failed
  fuzz      4 passed / 0 failed
  perf      5 passed / 0 failed
  cagara_syntax 51 passed / 0 failed
  (doc-tests: all 0 passed / 0 failed)
diff -r /tmp/cagara-baseline-frozen /tmp/cagara-after  ->  BASELINE_DIFF_EMPTY=YES
cargo clippy --workspace --all-targets -> clean (only "Git tree is dirty")
```

## SIGABRT resolution
The stack overflow (`mutual_application_is_reported_too`, `self_application`)
is gone; the two tests are listed `ok` in the 204/0 `cagara-hir` run.

Cause was frame size, NOT a guard hole — the guard fires correctly:
```
$ ./target/debug/cagara /tmp/mutual.cagara
/tmp/mutual.cagara:1:10: error: `f` evaluates more than 256 calls deep; recursion is not supported
```
Lead's measurement: `size_of::<Value>()` 80 -> 136 (`CoreTerm` 128, `Rel`
unchanged 80); `MAX_DEPTH` is 256 (`eval.rs:43`), so 256 frames x several
`Value`s x 56 extra bytes overflowed the 2 MiB test thread while the CLI's
8 MiB main thread survived — exactly the CLI-fine / test-SIGABRT split.
Fixed by shrinking the payload; `MAX_DEPTH` was deliberately NOT lowered
(it would change `examples/errors.cagara`'s frozen `.err`) and the test stack
was deliberately NOT raised.

## Attack C — closed
- CONFIRMED counterexample (`omit` missing key) retired: `checked.rs`
  now checks `input.row.has(&key)` and reproduces `schema.rs:245`'s wording.
- 22 adversarial compositions (`verify/checked_adversarial_probe.rs`) all pass.
- **Limit:** the row-vs-`schema` invariant is NAME-level only. Injecting the
  original `update` bug left the probe passing. See `attackE-nonvacuity.md`.

## FINAL gate — GREEN (verified after the prelude break was reverted)
```
cargo test --workspace
  cli        21 passed / 0 failed
  cagara_fmt 16 passed / 0 failed
  cagara_hir 206 passed / 0 failed
  lsp        20 passed / 0 failed
  cagara_sql 51 passed / 0 failed
  engines     2 passed / 0 failed
  fuzz        4 passed / 0 failed
  perf        5 passed / 0 failed
  syntax     51 passed / 0 failed
  total     376 passed / 0 failed
diff -r /tmp/cagara-baseline-frozen /tmp/cagara-after  ->  BASELINE_DIFF_EMPTY=YES (40 files)
cargo clippy --workspace --all-targets -> 1 warning, 0 errors
  crates/cagara-hir/src/eval/elaboration_tests.rs:528
  clippy::clone_on_copy: `kind.clone()` on a `SetKind` (which is `Copy`)
```

## Second regression (cleared): `prelude.cagara` half-landed set stages
Between my first green run (206/0) and this one, four `infixl` declarations for
set-operation stage shorthands landed without valid syntax:
```
$ ./target/debug/cagara examples/public.cagara
<prelude>:138:1: error: syntax error: expected `=`
 138 | infixl 1 &unionAll
```
```
cargo test --workspace -> tests/cli.rs: FAILED. 9 passed; 12 failed
diff -r ...baseline... -> 40/40 FILES DIFFER
```
`git diff --stat HEAD -- prelude.cagara` was `+24` lines. Most likely the
longest-match problem: `&unionAll` begins with `&union`, so `infixl 1 &union`
matched first and left `All` unconsumed — which then cascaded into four
`` `union` is defined more than once `` errors at `:137-140`. Reverted; prelude
parse and all 40 outputs restored.

**Why this matters for the final report:** this break was invisible to
`cargo build` (exit 0) and to `cargo clippy` (0 warnings on the broken tree!).
Only an actual `cargo test` run or a CLI invocation caught it. `cargo test
--no-run` also passes, because prelude parsing happens at *runtime*. So of the
four gates, only two are real: **`cargo test --workspace` and the frozen diff.**
`cargo build` and `cargo clippy` are necessary but cannot be cited as evidence
of correctness.
