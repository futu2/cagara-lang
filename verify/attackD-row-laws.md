# Attack D audit: the two row laws (Lead)

The `update` bug (verifier BUG #1) was a left-wins/right-wins confusion:
`RowType::merge` implemented the JOIN law (left-wins) and was reused for
`update`, whose law is right-wins (`check/reduce.rs:94-103`).

Audited every call site of the two row formers:

| site | former | law needed | law implemented | verdict |
|---|---|---|---|---|
| schema.rs:155 `Rel::Join` | join_columns | left-wins | left-wins | OK |
| schema.rs:209/237 `merge_columns` | merge_columns | right-wins w/ positions | positions kept, collision replaced by fs | OK |
| check/infer.rs:1958 `Cons::Update` | merge_columns | same | same | OK |
| check/infer.rs:2178 `Cons::JoinOut` | join_columns | left-wins | left-wins | OK |
| core.rs:244 `RowType::merge` | join_columns | left-wins (JOIN) | left-wins | OK for joins |
| checked.rs:335 `update` | RowType::merge | RIGHT-wins | left-wins | **BUG** |
| checked.rs:602-607 `join_row` | RowType::merge | left-wins (JOIN) | left-wins | OK |
| lower.rs:374 `Rel::Update` | merge_columns | right-wins | right-wins | OK |
| lower.rs:598 `Rel::Join` | join_columns | left-wins | left-wins | OK |

Conclusion: exactly one site conflated the two laws (checked.rs:335). The
evaluator/schema/lowerer paths are consistent — which is why BUG #1 is
confined to the new checked layer and no existing test caught it.

Distinguishers, for the doc comments:
* JOIN law  = `rules::join_columns`      : left wins on collision, right-only appended.
* MERGE law = `schema::merge_columns`    : left positions kept, collision takes the right value, right-only appended.
  (`merge_columns` returns names only; the *type* half of the same law lives in
   `Cons::Update` at infer.rs:1959-1972 and must not be reimplemented by hand.)
