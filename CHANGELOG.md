# Changelog

## Unreleased

- Remove the `merge` prelude definition. It had no relational lowering, so any
  well-typed use of it ended in an internal error; `merge r s` is a row *type*
  former, used by `update` and the joins, and two queries are combined with a
  join or a set operation. `primitive "__merge"` stays in the primitive table and
  is still refused as a compiler fault.
- Remove the unreachable `decimal` scalar type from the checked layer. A name in
  a type position that is not one of the built-in constructors is a type
  *parameter* (signatures bind theirs implicitly), so `decimal` never produced a
  scalar type of its own; `docs/LEARN.md`, `docs/CAGARA.md` and `docs/PLAN.md` now
  state that behaviour instead of claiming a decimal type exists.
- Delete code that no pass reached: the arena's introspection surface
  (`QueryNode`, `CompiledQuery::{root, row, node, nodes, chain, names}`,
  `QueryNodeKind::arity`, `Payload::is_prefix`, `Builder::{with_budget, row,
  charge}`), the per-node budget charge (the normalizer charges as it reduces),
  `Value::is_identity` and `normalize/thunk.rs`, `Verified`,
  `TemplateNodeKind`/`template_node_kind`, `ScalarType::{is_maybe, pow_name,
  strip_maybe, elem}`, `Error::is_unsupported`, `core::Diagnostic`,
  `TypeCheck::result_expr`, `Ops::MIN_LEVEL`, `Budget::{depth, spend_node}`,
  `Env::is_empty`, and `_frame_of`.
- Accept every `mapValue` wrapper the type-level table names. The arena decoded
  the label with its own `match`, which knew `nullable` and `list` but not `id`,
  so `q & mapValue "id"` type-checked and then failed at lowering; the stage now
  reads the checker's one table (`check::reduce::value_map_of`).
- Drop `DefinitionKey` and `CompiledQuery::key`. Reuse is decided by comparing
  exact per-definition facts, so the source hash the compiler computed for every
  root definition on every compile had no production reader.
- Correct documented claims that the compiler contradicted: `%` is `int` only,
  `as` is optional on `import`, `--dialect` refuses `fullJoin` for MySQL, the
  nullability and window diagnostics are quoted as emitted, outer-join filters
  stay in a derived table (only inner-join inputs inline), `omit`'s key may pass
  through a helper's parameter, name rewrites cannot collide, `mapValue` is a
  writable stage, `fmt` leaves a `;`-terminated item verbatim, `$` is the loosest
  operator and `<<<` shares `>>>`'s level, and the date/string intrinsic split.

- Add `nothing` as the typed SQL NULL value; `just` continues to lift a
  non-null value into `maybe`.
- Allow an optional top-level `;` between items, and treat a column-0
  identifier that is not a definition header as a continuation with a targeted
  layout diagnostic instead of a confusing parse error.
- Add local `let name = value in body` bindings. They are lexical,
  monomorphic, and inlined during normalization, so they add no SQL. The
  formatter and the language server (hover, go-to-definition, references,
  completion) understand the binding name.
- Add explicit nullable aggregate helpers (`sumMaybe`, `avgMaybe`, `minMaybe`,
  `maxMaybe`, `countOfMaybe`, `countDistinctMaybe`). They pass the nullable
  column to the SQL aggregate unchanged, preserving SQL's null-skipping
  semantics; coalescing first would change `avg`/`min`/`max`.
- A definition named `main` that is not a `query` is now a compile error
  instead of a silent no-output success. A file with no `main` is still a
  library, and `--only` can still inspect a scalar helper.
- `toInt`/`toFloat` and the `try*` casts now target the language's widths: a
  64-bit integer and a 64-bit double, spelled per dialect. `toInt` of a float
  truncates toward zero instead of inheriting PostgreSQL/MySQL cast rounding.
- Lower an empty `inList []` to a constant `FALSE`/`TRUE` rather than invalid
  `IN ()` SQL, and reject `fullJoin` for the MySQL family, which has no
  `FULL JOIN`.
- Add math helpers (`abs`, `round`, `roundTo`, `floor`, `ceil`, `sqrt`,
  `power`, `sign`, `greatest`, `least`), `toString` for `bool`, `tryDate` /
  `tryTimestamp`, and the window helpers `firstValue`, `lastValue`, `ntile`,
  `percentRank`, and `cumeDist`.
- Document field-tail open rows (`query { id = int | r }`) as the way to
  annotate column-reading helpers.
- Add filtered aggregate helpers to the prelude (`sumFilter`, `avgFilter`,
  `minFilter`, `maxFilter`, `countFilter`, `countOfFilter`,
  `countDistinctFilter`). They are the portable `CASE` equivalents of SQL
  `FILTER (WHERE ...)` and keep the unfiltered aggregate's result type.
- Reject self- and mutually recursive definitions during type checking, even
  without an annotation. Previously an unannotated cycle could check as a
  fresh type variable and produce no SQL.
- Accept a leading UTF-8 BOM on input; `cagara fmt` preserves it.
- Render diagnostics with character columns and clip very long source lines.
- Write formatted files through a sibling temporary and rename.
- Publish LSP diagnostics for the edited document immediately and refresh other
  open documents after a short debounce, instead of recompiling every document
  on every keystroke.
- Fix the README example (`sum` is nullable), compile the README example in CI,
  and check the version references in `flake.nix`, `CAGARA.md`, and the VS Code
  README.
- Replace the stale "declare operators in prelude.cagara" guidance in
  diagnostics, comments, and editor syntax files with the fixed-table rule.
- Fix rustdoc links, add a rustdoc CI gate, and add Windows/macOS compile
  checks plus a scheduled RustSec audit.
