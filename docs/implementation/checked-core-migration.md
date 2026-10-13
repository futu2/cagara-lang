# The checked-core migration: one implementation of the language

**Status:** complete. Every query is built from a normalized term. The source walk
that used to build them is deleted.

This is the record of what the migration claimed, what it measured, and what it
found. It exists because the *evidence* is the reusable part: the next person to
touch the query path should be able to re-run these checks rather than trust them.

## What changed

Before: a definition whose body was a query was elaborated by **re-walking its
source**. Applying a function built an environment and walked the callee's body
again, from the AST. That one decision was the root of a family of workarounds,
each existing only because there was no separate normalization pass — a bare alias
had no body to re-walk, a composed function value had no body either, a callee was
classified by its *syntax shape*, and a polymorphic definition was re-elaborated
once per hole assignment.

After: `source -> syntax -> type checking -> normalization -> CheckedQuery ->
erasure -> Rel -> SQL`. Normalization resolves every name, inlines every
definition and reduces every application, so the lowering sees a saturated tree
with no syntax left to re-read.

```text
crates/cagara-hir/src/elaborate.rs   2329 lines -> 270
```

`elaborate.rs` now decides whether a definition is a query and hands it over.
Nothing in the crate re-reads source to decide what something means.

## Why the term layer was the right target

The reconnaissance that decided the shape of the work: **all 33 query-construction
shapes normalize to terms with zero gaps.** The term layer already knew
everything; what was missing was a *reader* — a lowering from `Term` to the
checked constructors. That reframing turned a language-wide change into one
function, and it is the same reframing that fixed the `__table` row problem
below.

## The four findings worth keeping

### A node's type is not its definition's signature

`CheckedQuery::table` refuses to build without columns, and a `__table` term
carries two strings. The obvious source of the columns — the declaring
definition's signature — is **wrong**, and measurably so:

```cagara
users & innerJoin orders (.<id == .>uid)
  __table -> ["id", "uid"]     (orders)
  __table -> ["id", "name"]    (users)
```

No definition signature can produce both. The row comes from the **application
node's** recorded type.

Reading it was also subtler than it looked: `use_tys` stored a *shallow* resolve,
so an application's `query r` kept `r` as a variable and `closed_fields` answered
`None` — while the *printed* map, which zonks, displayed the row. Two readings of
one fact disagreed. Storing the zonked type fixed it, and
`every_node_records_its_whole_type` pins it.

### Stage values need no lowering at all

The plan expected this to be the subtle step. Measuring first said otherwise:
applying a stage **is** the reduction the normalizer already performs.

```text
s = where p >>> select f   ->  Lambda [x] -> __select [f, __where [p, x]]
users & s                  ->  __select [f, __where [p, __table]]
users & (x -> where p x)   ->  __where [p, __table]
```

Three source-path functions existed to spell that application three ways; here it
is one. `a_stage_value_lowers_like_its_inline_spelling` writes one pipeline five
ways and requires all five to lower to the identical query.

### Operand order is read, not guessed

The two spellings of a set operation put the piped query on **opposite sides**,
and both are correct:

```text
users & union vips   ->  __union [vips, users]     (piped query last)
users &| vips        ->  __union [users, vips]     (piped query first)
```

`union` is `query r -> query r -> query r` at the pipe's own level; `&|` is
`q => r => union q r`, where the body re-binds. Inferring the side from which
spelling was written is exactly the bug
[`FIXED-piped-set-operand-order.md`](FIXED-piped-set-operand-order.md) records, so
the lowering reads `args[0]` because that is what the term says.
`the_two_set_spellings_really_do_differ` asserts the two produce *different*
trees, so their both matching is evidence rather than coincidence.

### `limit`/`offset` state a rule, not a capability gap

`Gap` carried only an origin and a message, so every checked-constructor rejection
became `Fault::Compiler` — "this phase cannot build that" — and a program mistake
like `where (.nope > 0)` was routed around instead of reported. `Gap` now carries
the fault, and the constructors' verdicts survive. **The wording never changed;
the classification decides who hears it.**

## How the switch was shown to be complete

Three measurements, in the order they were needed:

1. **The source walk panics if reached.** With the fallback still in place, made
   the walk panic and ran the whole suite: **nothing reached it.**
2. **A fallback that records is caught.** Disabling the term route makes the gate
   report **40 fallbacks across 7 programs** (18 whole queries, 22 stage
   discards); making a definition record a single discard fails it with the
   reason and a count, so its green is not vacuous.

   **What this does not cover**, and it took a failed simulation to see it: the
   sieve is empty when nothing *records* a discard, so this catches a fallback
   that declares itself or a construct the term route rejects. It would not catch
   a *silent* misroute — a second route that built a query without declining or
   recording anything. My first attempt at testing this gate did exactly that and
   the gate correctly passed, because there was nothing to report. `elaborate_def`
   has no branch — it calls the term route and returns the result — so no such
   route exists, but the gate is evidence about the fallbacks that exist rather
   than a proof that none could.
3. **The compilers agree byte-for-byte.** Building `70eb285~1` in a worktree and
   diffing its output against the current compiler on all four examples: 12 SQL
   statements and 3 diagnostics, **identical in text, line, column and wording**
   — the only difference is the working-directory prefix.

No snapshot changed at any point in the migration, which is the user-visible
statement: same SQL, different producer.

## The failure mode this migration kept hitting

Five separate times, a comment described a world that had ended — twice in the
direction that would have made a reader believe a safety net existed when it did
not. One gate asserted that "the source path is unreachable" while citing a test
that had been deleted; another claimed to compare two implementations after there
was only one.

Two tests were also **green without checking anything**:

* searching the crate's source text for a primitive's name passes on a *comment*;
* writing `__agg` as a program body normalizes nothing, because it is not a name
  the checker knows — the scan inspected **zero** stages and "nothing is
  unlowered" was true because nothing was looked at.

A floor on the inspected count is what caught the second. Both are corrected, and
the lesson is in the tests: **an assertion that passes when the scan finds nothing
is not evidence.**
