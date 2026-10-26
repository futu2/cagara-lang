# Changelog

## Unreleased

- Formatting sends the lines that changed, not the whole file. The language
  server answered `textDocument/formatting` with a single edit spanning line 0
  to the end, and an editor pays for that edit in full: every token and fold
  invalidated, an undo entry the size of the file, the cursor and selection
  moved, and — with a vim or neovim client mirroring the buffer elsewhere —
  every line pushed back again. Formatting one definition therefore cost the
  editor what retyping the file costs, however fast the formatter itself is
  (8 ms on `prelude.cagara`, 49 KB, and nothing at all on a file that is
  already formatted). The edits are a line-level patience diff now: a line
  occurring exactly once in each text anchors the two, and the gaps between
  anchors are compared again, so a re-laid-out definition is one small edit
  and its neighbours are in no range at all. A 56-line file whose third
  definition needs laying out goes from one 56-line replacement to one
  one-line replacement.

- Type checking is memoized per definition, so a keystroke re-checks the
  definition it touched rather than the file it is in. A module used to be one
  salsa query: any edit re-ran inference over every definition, 140 ms at a
  thousand definitions, whether one digit of one literal had changed or not
  (measured: the same 1000 definitions split across ten modules re-checked in
  17 ms instead of 180 ms). Each definition is now an input of its own — its
  name kept apart from its body, so editing a body leaves the module's name map
  alone — and `def_check` checks one definition, reading the memoized check of
  any definition above it. A reader therefore depends on the definition it
  read, not on the whole file. The language server's keystroke went from 206 ms
  to 42 ms at a thousand definitions, and from 19 ms to 3 ms at a hundred.

  A name defined *below* its use is still checked inline, as before: that is
  what lets a forward reference close a cycle and report `q = q` as the
  recursion it is, where routing it through salsa would make it a salsa cycle
  instead. The depth guard stays where it was too, and now attributes its
  diagnostic to the definition whose chain is too deep, with the module keeping
  only the first such diagnostic so one broken chain is reported once.

  Parsing is still per file, and so is collecting a module's results, so a
  keystroke still costs a parse (about 8 ms per thousand definitions). An edit
  that changes a *type* also re-checks more than its direct readers, because
  every check reads the module's name map; `docs/PERFORMANCE.md` records both.

- The language server no longer queues keystrokes behind its own analysis. It
  ran on one thread: a change was checked and its diagnostics published before
  the next message was even read, so ten keystrokes typed in a burst cost ten
  full checks of the file — 2.3 s of backlog on a thousand definitions — and a
  hover asked meanwhile waited for all of it. The message loop now only reads
  and routes; an analysis thread owns the workspace, and the work it is handed
  is collapsed before it runs: superseded `didChange`s for a document are
  dropped unread, because their text no longer exists anywhere. A burst of ten
  keystrokes settles in 0.4 s instead of 2.3 s, and no document waits behind
  another's copy.

- Publishing every open document no longer recompiles them all. The compilation
  cache held one root — whichever was current — so switching roots (which
  publishing each open document does) discarded the previous result: an edit
  cost one compilation per open document, 350 ms per save at four documents.
  Caches are keyed by root now, and dropped only for roots that can reach the
  edited module through imports. Publishing a document that did not change is a
  cache hit, so the same save is 0.5 ms.

- A save or a watched-file event re-reads the files in place instead of
  rebuilding the graph. Rebuilding keeps only the texts held in memory and
  throws away every memoized parse and check, including each open document's
  compilation, which the publish that follows then redid from scratch. Files
  whose text is unchanged are now left alone, and a changed one is checked again
  like any edit; only a changed import list still needs the graph rebuilt.

- Column completion no longer rebuilds the workspace for every `.`. Probing for
  the columns at the cursor checked a copy of the whole graph — a new database
  with every module re-added and the prelude re-parsed — which cost 345 ms on a
  thousand definitions, and cost it again for a request asking the same question
  twice. The probe is one dedicated module inside the live database, sharing the
  memoized parses and checks of the modules it imports: 163 ms for a new text,
  0.3 ms for a repeat.

- A call whose last argument is a parenthesised condition no longer strands the
  arguments before it. Hugging the paren printed `ifThenElse (` on the call's
  line, the condition indented under it, and `) (toYuan .lstbal)
  (toYuan .balance)` on the line after the `)` — the arguments after the
  bracket were only visible by counting from a `)`. The hug is now taken only
  when the hugged parts fit, so such a call packs its arguments like any other,
  and two record fields that differ by a few characters no longer get
  different shapes.

- A lambda whose body is a pipeline moves the pipeline down after the arrow
  when it does not fit, with the `&` stages flush with the head:

      name = customer_number =>
        some_query
        & where (.customer_number == customer_number)

  Instead of hugging it, which left `=> some_query` on the definition's line
  however wide it grew — nothing measured that first line, so a long head ran
  past the width. A short pipeline body still stays on the arrow's line, and a
  definition's own pipeline is unchanged.

- `cagara fmt` no longer shreds a long application. Arguments used to be
  laid out all-or-nothing: one column past the width and every argument got a
  line of its own, so 18 arguments became 19 lines where two would do. They now
  fill the lines after the head, breaking only where the next argument would not
  fit — and a body that fits once it has moved down is not split at all.
- A `case` used as a definition's body now moves down whole when it does not
  fit, instead of hugging `x = case {` and spending the rest of the width on
  nothing: `x =` on one line, then `case {` and its arms indented under it. A
  short `case` stays on the `=` line, and a record field's `case` still hugs
  (`b = case { ...`), which is the shape the reference writes.

- A lambda's parameter can carry a type annotation: `(x : string) => body`.
  The annotation is checked, not noted — the body is typed with the parameter at
  that type, and a use has to supply it, so `(x : string) => x + 1` is the
  overload error it looks like and `users & named`, where `named` reads columns
  the argument does not have, is reported rather than passed along. The
  parentheses are required: `x : string => body` stays a syntax error, because
  a `:` cannot otherwise say how far it reaches (`f x : string => body` would
  annotate the application's argument, or its result, or the lambda), and the
  bare form would only be available where an expression starts. `let` still
  binds without one; write the lambda it stands for.

  A type variable in the annotation is *filled in* by the argument rather than
  promised for every type: `(least : expr r int) => q => where (.age >= least) q`
  infers `expr { age = int | r } int -> query { age = int | r } -> query { age = int | r }`,
  the row it was given. That is the same text a signature would use, read the
  one way a parameter can mean it, and it is what makes an annotation useful
  here at all: a signature's `r` is rigid, and a rigid row clashes with the row
  of the first column a body reads.

  The parameter is now a boxed `ast::LambdaParam` — its name, its own span, and
  its optional annotation — so the language server finds the name inside the
  parentheses for hover, go-to-definition, references, and highlights, and an
  expression node still costs what a name does. Writing `x : int => body`
  reports the form it wants instead of "unexpected tokens after definition".
- Document a table's columns, one level below the definition that names them. A
  column is a field of a record type, so it cannot be an item with a `##` block
  of its own; the block now goes above the field, inside the signature:

      users : query {
        ## The primary key.
        id = int,
        ## Display name; two people may share one.
        name = string
      } = table "public" "users"

  `cagara doc` prints those notes as a `Fields:` list under the table, and
  `--json` carries them as a `fields` array with the undocumented ones present
  and `null`, so a tool asking what columns a table has does not have to infer
  the gap. The two levels are independent: the block above `users` is the
  table's, the block above `id` is that column's.
- `cagara doc --check` reports a record type that documents some of its fields
  and not the rest, naming each remaining field and its line. A record with no
  field documentation at all says nothing — a table whose columns are
  self-evident is not a gap, and warning about every field of every signature
  would bury the definitions that do need writing. Still warnings, still exit 0.
- A record type's fields now carry their own spans in the AST
  (`ast::RecordField`), which is what attaches a doc block to a field. Nothing
  else about the AST changes, and the two places that convert a record
  annotation to a type read the field through the struct.
- Hover a column's documentation. A column is a field of a record type, so the
  type checker has nothing to point at — unification matches shapes, not
  declarations, and the table a use came from is gone by the time the use is
  typed. What survives is the program's shape: the row a column is read from is
  the *input row* of the stage it sits in, and that input is the query the stage
  is piped from. The language server follows that chain down through the steps
  that keep their columns (`where`, `order`, `limit`, `offset`, `distinct`),
  reads the column's block from the declared record type of the definition it
  reaches, and appends it to the type it already showed. `.>x` in a join
  predicate follows the join's own query, `.<x` the one piped into it, and a
  column inside a `select`/`update`/`agg` argument is read from that stage's
  input row — which is where those dots actually point. `t = aam.tbl` is a step
  rather than a declaration (it says nothing about the columns), so the walk
  follows its body instead of stopping there, and the `&…` shorthands (`&?`,
  `&.`, `&-`, `&|` …) carry their stage inline and read the piped query's row
  like every other stage.
- Hovering a field where it is *declared* — `age` in
  `users : query { age = int }` — shows its block as the whole tooltip. A field
  has no type of its own to report, and the line beside the cursor already says
  it, so the documentation is what the reader is asking for.
- Nothing is shown when the walk reaches no declaration: a column a `select`
  computed, one `prefix` or `agg` named, or one a join merged has no single
  place that named it, and attributing it to another table's identically named
  column would put the wrong words on the screen. The hover is then exactly what
  it was: the type.
- The examples document their columns (`schema.cagara`'s one table,
  `report.cagara`'s two), and `tests/snapshots/pipeline_golden.txt` is
  re-recorded for it: the emitted SQL and the pipeline output are unchanged, and
  only the byte spans moved, because the generated programs' signatures are now
  written over several lines. Checked the same way as before, by normalizing
  `start:`/`end:` away and diffing.

- `cagara fmt` lays a multi-line `case` out the way §4.5 of `CAGARA.md` writes
  one: `case {` ends its line, every arm and its `;` gets a line of its own,
  and `}` closes the last one. It used to hang the first arm off the `case {`
  line and leave `}` on the last arm's, so the reference's own example came back
  reformatted into a shape the reference does not show, and a `case` written one
  arm per line was not stable. A `case` that fits still stays on one line.
- A `case` inside a record is no longer folded into `{ a = 1, b = case { arm;`
  on one line: a record whose field value is a `case` written over several lines
  (or holding a comment) breaks one field per line, so the layout survives a
  format. The `case` itself is kept on its field's line and indents its arms
  from there.
- An application's last argument no longer gives up being hugged because of a
  comment *inside* it: only a comment between the argument's own braces breaks
  the call, which is what that comment is about. `select { ... }` with a comment
  in a field keeps `select {`.
- A `case` arm's value is indented from the arm, so a nested `case`'s or
  record's `}` can never land at the outer arm's level, where it would read as
  the outer `case`'s own brace.
- A lambda's layout is decided by width alone, and now has tests saying so:
  `f = a => body` is one line while it fits, whether or not it has a signature
  and whether the source put the body on the `=>` line or the next one. When it
  does not fit, the parameters stay on the `=` line and the body moves to an
  indented line; a body that brings its own lines (`case`, a record, a
  pipeline) is hugged and breaks itself, so `a =>` is never left alone.

- Documentation that lives with the definition it describes: `##` above a
  definition documents it, `#!` above the first item documents the file, and
  both are ordinary comments to the compiler — a file without them means exactly
  what it meant before. `cagara doc <file>` prints the reference (a section per
  name, with the inferred signature and the `##` text), `--json` prints it for a
  tool, and `cagara doc --prelude` prints the built-in prelude's own. Hover now
  appends the same text, so a definition is written once and read in the editor,
  in the reference, and in the generated JSON.
- `cagara doc --check` lists the top-level definitions that have no `##` block.
  It **warns and exits 0**: a missing doc comment is a writing task, not a
  compile error, and a project should be able to grow documentation without its
  build turning red over what is not written yet. Adjacent definitions of one
  name are reported and printed as one entry, because an overload set is one
  thing to document.
- The prelude is now documented definition by definition, which is what
  `cagara doc --prelude` prints: 156 entries covering every name in §7 of
  `CAGARA.md`, with the reasoning that used to live in section headers next to
  the definition it explains. The examples document their definitions too, and a
  test keeps both that way.
- `crates/cagara-hir/tests/snapshots/pipeline_golden.txt` is re-recorded. The
  pipeline output is unchanged; what moved is the positions. Prelude byte spans
  (module 0) follow the new comment lines, and the diagnostics for
  `examples/errors.cagara` and `examples/public.cagara` name the new line
  numbers, since documenting those files added lines above the code. The
  re-record was checked by normalizing `start:`/`end:` away and diffing: the
  only remaining differences are those line and column numbers.

- Hover a column as the expression type it has (`expr r string`, or
  `expr (join r s) int` for a side-qualified column) instead of the bare value
  type. A column is row-phase, which is what an `expr r a` parameter accepts, and
  `string` hid both the phase and the row: a reader copying it into a helper's
  signature wrote `string -> string`, which the checker refused. A literal keeps
  the type it was lifted to, because it may be a label (`limit 2`) rather than an
  expression.
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
