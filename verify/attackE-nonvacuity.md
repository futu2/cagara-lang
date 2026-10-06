# Attack E: non-vacuity of the negative tests (verifier)

For each negative test, does it fail for the **stated** reason? A test asserting
only `is_err()` on input rejected by an earlier/other check does not prove the
rule it names.

Method: for every `#[test]` in `checked/tests.rs` and `core_term/tests.rs`
containing `is_err()`/`is_ok()`, check whether it also asserts a message or
otherwise discriminates which rule fired.

## WEAK TESTS (pass, but prove less than the name claims)

### E1 — `a_window_spec_checks_its_keys` (`checked/tests.rs:952-955`)
```rust
#[test]
fn a_window_spec_checks_its_keys() {
    let a = CheckedExpr::agg_template("COUNT(*)".to_string(), vec![], ScalarType::Int, o()).unwrap();
    assert!(window_spec(vec![a], vec![], None).is_err());
}
```
Only `is_err()`. The input is an **Agg-phase** expression in the `partition`
list, so `WinSpecChecked::check` (`checked.rs:1111-1119`) calls
`place(Place::Key, e)` → `rules::place` (`rules.rs:82-85`) returns
`"sort and partition keys must be plain column expressions; ..."`.

The test would **also pass** if `place` rejected for an unrelated reason, or if
`window_spec` failed earlier. It never asserts the message, so it does not
distinguish "the key rule fired" from "something else rejected it". The `order`
loop at `checked.rs:1115-1117` is also never exercised — only the `partition`
path is.
**Fix:** assert the message contains `"partition keys must be plain column"`,
and add an `order`-list case.

### E2 — `omit_and_schema_agree_on_a_missing_key` (`checked/tests.rs:793-800`)
Two bare `is_err()`s. Weak *on its own*, but the immediately preceding
`omit_rejects_a_key_the_input_does_not_have` (`:785-790`) pins the exact message
and origin, so the rule **is** covered by the pair. Listed for completeness, not
a real gap.

### E3 — `core_term/tests.rs:461-470` (aggregate-mixed-with-row phase clash)
```rust
assert!(CoreTerm::tpl("$1 + $2".into(),
    vec![CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")]).unwrap(), col("b")])
    .phase().is_err());
```
Only `is_err()`, while the sibling assertion directly above it (checked/tests.rs
equivalent and `checked.rs:385-389`) *does* assert `.contains("window")` for the
other clash. The comment says "a clash, not a phase" but nothing pins which
`rules::clash` arm fired (Agg+Row vs Agg+Win vs Win+Win). Weak.

### E4 — structural tests with `is_err()`
`a_checked_module_carries_its_definitions_and_choices` (`:865`),
`a_checked_program_gathers_every_module` (`:1010`),
`a_checked_program_reports_the_same_diagnostics_as_the_checker` (`:1058`),
`checked_program_from_type_check_reuses_a_check_already_run` (`:1074`).
These are not rule tests; `is_err()`/`is_ok()` is the right assertion for them.
**Not weak** — recorded so the heuristic above is not misread.

## NON-VACUOUS (verified, good)

- `a_frame_rejects_impossible_bounds` (`:945-948`) asserts **both** directions
  (`Following(2),Following(1)` → `is_err`; `CurrentRow,CurrentRow` → `is_ok`),
  so it cannot pass if `frame` were an unconditional `Err`.
- The three tests that FAILED in the report-#7 run and now pass —
  `core_term::tests::table_columns_mut_descends_through_at_for_attach_schema`,
  `eval::elaboration_tests::set_stage_takes_left_then_right`,
  `eval::elaboration_tests::where_order_limit_elaborate_to_distinct_constructors`
  — are proven non-vacuous by having failed: they detect a real defect.
- `select_stage_takes_its_fields_first` and siblings assert field **names,
  order and the input table**, not just node kind, so an argument-order
  inversion fails them (verified by injecting the inversion).

## Honest limit of the whole `erase`/`schema` invariant (from report #8)

Injecting the original `update` bug (`overwrite` → `merge`) into `checked.rs`
left my adversarial probe **passing**, because `schema::schema` returns column
*names* only (`schema.rs:8`). Name-level agreement cannot detect a wrong column
type. Only `update_takes_the_new_type_of_an_overwritten_column` and
`update_keeps_positions_on_a_same_type_overwrite` caught it.
