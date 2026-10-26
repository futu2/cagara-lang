# Catalog scale and incremental compilation

The cache preserves useful work across edits without putting mutable state in
the checked relational tree. It shares immutable module checks and name maps,
borrows imported schemes, and reuses erased queries only when their exact
checked facts and dependencies are unchanged. Inference still runs per definition within a module.

## Reproducing the measurements

Run the opt-in benchmark through the development shell:

```sh
CAGARA_BENCH_DEFINITIONS=1000000 CAGARA_BENCH_LAYOUT=import \
  nix develop -c cargo test --release -p cagara-hir \
  large_definition_cache_benchmark -- --ignored --nocapture
```

`CAGARA_BENCH_LAYOUT=root` puts all generated definitions in the root file.
`import` puts them in `catalog.cagara`, imported through an alias by a small
root file containing `q = catalog.d0`. Files are in-memory overlays; the
benchmark does not create a catalog on disk. The count defaults to 100,000
and must be at least two.

Each table has one integer column. The root layout changes the first table's
name without changing its byte length. The import layout changes only the
query, from `catalog.d0` to `catalog.d1`. The benchmark checks query counts,
diagnostics, zero elaborations on warm requests, and exactly one root-query
elaboration after the edit. Regular tests separately compare cached and fresh
results across dependency changes, overloads, errors, and shifted source spans.

Phases are measured separately. Compilation follows an already completed
check and includes returning an owned `Compilation`, which clones the cached
relation trees. Warm diagnostics use `compile_diagnostics`, avoiding that
clone. A module's diagnostics are rendered inside its memoized check, so a
`check()` on an unchanged module clones the rendered messages rather than
re-rendering each one against the text. Returned checks and compilations are
dropped before editing; a client retaining older results would use additional
memory. Source generation, edit string construction, test assertions, and
result destruction are outside the listed phase timings. Workspace destruction
is reported separately by the harness. Linux `VmHWM` records process peak
resident memory, including the harness, across both cold compilation and
editing.

## Measurements

Measured on 2026-10-08 at commit `a55631f`, in the Nix development shell with
Rust 1.98.1, optimized release builds, Linux x86-64, an AMD Ryzen 5 5600X,
and about 63 GiB RAM. Each column is one fresh process, run sequentially.
These are single samples of the compiler API, not LSP transport or UI latency.

| Phase | Root: 100k tables | Root: 1M tables | Import: 100k tables | Import: 1M tables |
| --- | ---: | ---: | ---: | ---: |
| Load and resolve | 646 ms | 7.65 s | 665 ms | 7.75 s |
| Cold type check | 857 ms | 9.21 s | 876 ms | 9.24 s |
| Cold compile, owned result | 403 ms | 4.96 s | 14.0 ms | 226 ms |
| Apply source edit | 1.10 s | 12.12 s | 0.030 ms | 42.1 ms |
| Edited type check | 984 ms | 11.48 s | 0.022 ms | 14.0 ms |
| Edited compile, owned result | 389 ms | 5.21 s | 0.012 ms | 1.93 ms |
| Total measured edit phases | 2.47 s | 28.82 s | 0.064 ms | 58.0 ms |
| Peak resident memory | 933 MiB | 9.30 GiB | 408 MiB | 4.07 GiB |

Warm unchanged checks averaged 78–97 ns per API call and empty cached
diagnostics 11–13 ns over 100 repetitions in these runs. These tiny timings
measure cache retrieval only; they do not measure an editor interaction or
requesting owned copies of every query tree.

Before sharing name maps and borrowing elaboration scopes, the same release
benchmark at `81ae49e` took 28.3 ms to apply the small root edit over a 100k
import and 10.5 ms to compile it. That measurement identified catalog copies
on both paths. Earlier debug-profile measurements use a different harness
and are not a release-performance baseline.

## The checked-core migration costs cold compilation

Measured on a single machine — an AMD Ryzen 9 5900HX, Rust 1.99.0, release
builds, 100k tables, both layouts. The two compilers were built from `a2b1773`
(before the migration) and from the current commit, and run in the same session,
so the columns are comparable to each other even though they are not comparable
to the table above, which used slower hardware.

| Phase | Root: before | Root: after | Import: before | Import: after |
| --- | ---: | ---: | ---: | ---: |
| Load and resolve | 825 ms | 845 ms | 794 ms | 802 ms |
| Cold type check | 1.04 s | 1.15 s | 1.04 s | 1.14 s |
| Cold compile, owned result | 463 ms | 666 ms | 43 µs | 42 µs |
| Edited compile, owned result | 492 ms | 522 ms | 18 µs | 17 µs |

Two increases, both attributable to a single deliberate change each.

**Cold type check, +10%.** The checker now stores each node's *deeply* resolved
type (`self.zonk(&t)` in `check/infer.rs`, where the tree's facts are recorded)
where it previously chased one variable chain and stopped (`self.resolve(&t)`).
That is the whole diff in
`check/infer.rs` — `git diff a2b1773 70eb285` shows it as one line. It is not an
oversight: with a shallow resolve, an application's `query r` kept `r` as a
variable, so a `table`'s columns were recorded and then unreachable, and every
consumer had to recover them from the declaring definition's signature — which is
wrong when one definition inlines two tables. The deep type is what makes a node
answer for its own row.

**Cold compile, +44% on the root layout.** Every query is now built by normalizing
to a term and lowering, instead of by walking source once. Splitting that cost
over the benchmark's 20k tables, warm and measured once: normalization **31.1 ms**
(1.56 µs per definition), lowering **2.3 ms** (116 ns per definition). The
term-building walk is essentially all of it; the lowering this migration added is
under 7% of the pair.

The generator's bodies are the best case for this: `table "public" "tN"` is one
application and a leaf. Per definition, that body normalizes in **1.6 µs**, while
`table & where` costs 12.6 µs, `table & select & order & limit` 30.9 µs, and
`table & agg` 13.0 µs — so the cost tracks the number of **stages in the body**,
not the number of definitions in the catalog.

Measured directly on one fixed program, normalizing and lowering a pipeline of
`table` plus N alternating stages, warm, averaged over 200 iterations:

| Stages | Per iteration |
| ---: | ---: |
| 1 | 4.7 µs |
| 2 | 9.4 µs |
| 3 | 18.0 µs |
| 4 | 24.8 µs |
| 5 | 34.6 µs |
| 6 | 47.1 µs |

That averages **8.5 µs per stage** from 1 to 6 stages, and it is the number a real
program pays. The growth is **not** linear: the increments are 4.7, 8.6, 6.8, 9.8
and 12.6 µs, so the last stage costs well over twice what the first does. Depth
compounds rather than adding, which is the expected shape — each stage's row is
computed from the previous stage's, and the terms nest.

Either way the 31 ms of normalization over 20k trivial tables is a floor rather
than a representative figure; a catalog whose queries chain six stages each would
pay several times that.

Two optimizations were tried and **rejected by measurement**, both the obvious
ones: sharing a single `Normalizer` across definitions so its inlining cache
survives saves 1.3 ms of 31 ms, and the resolver and the hole-assignment lookup
together account for about 2 ms of the 31 ms. The cost is the term construction
itself — allocating a `Term` per node, with its origin and type — rather than
repeated work around it.

The import layout is unaffected, which is the column that says what the cost is:
its root query is one definition, so nothing scales with catalog size. The
migration's cost is per *root query compiled*, weighted by that query's stages.

Worth knowing if a catalog ever compiles many root queries with long pipelines on
a cold cache. There is no fix to reach for that keeps the property the migration
exists to have; avoiding a term for a trivial leaf would reintroduce a second
route, which is exactly what the migration removed.

## What the cache guarantees and what remains linear

- Unchanged checks share immutable module results. A small importing module
  reads the schemes it uses without copying all imported schemes. Workspace
  updates share unchanged export and scope maps; elaboration borrows scopes.
- Exact per-definition facts include source, locations, closed types,
  expression ids, overload choices, and bindings. Changes propagate through
  an iterative dependency graph. Reuse is authorized by comparing those facts
  and nothing else: there is no hash identity, so no collision can stand in for
  an unchanged definition, and no source hash is computed per compile.
- Only successful unaffected query results are reused. A definition that only
  *moved* is unaffected: the facts that authorize reuse do not include where it
  sits, and the reused relation's `Rel::At` locations are translated onto the
  current text by the definition they were written in (`incremental::relocate`).
  A move that cannot be translated — a module whose definition list changed, so
  an index no longer names the same definition — refuses reuse and rebuilds.
  This is what makes typing cost one definition rather than a file: an inserted
  character shifts every definition below it, which used to look like an edit to
  every one of them.
- A cold load still parses and checks the entire imported catalog. Editing
  the catalog itself still reparses and rechecks that module. The root layout
  also walks all root definitions and returns all query results after an edit.
  One re-elaboration therefore does not imply constant-time compilation.
- The import measurements use a namespace alias. An unaliased import merges
  exported names into the importing module's scope, which remains proportional
  to the number of exported names when that scope is rebuilt.
- Field completion checks a hypothetical text (the file with a marker where
  the cursor is) in a dedicated module input inside the live database, so the
  modules it imports are the ones already memoized and the live graph is not
  rebuilt. The first probe of a file parses it and checks every definition;
  later probes reuse the inputs of every definition the marker is not in. These
  measurements do not establish fast field completion over a million-table
  catalog, since even one probe is linear in the file. Large symbol lists and
  error-heavy files are also outside this benchmark.

For large generated catalogs, keeping definitions in imported modules allows
small query edits to use the existing cache effectively. Edits inside one
module are memoized per definition (`def_check`), so an edit re-checks the
edited definition and the checks that read it; parsing is still per file, and
so is the aggregation of a module's results.

## The language server

The measurements above are of the compiler API. An editor adds transport, and
it used to add something worse: one thread that checked a change before reading
the next message, a compilation cache with a single slot, and a graph rebuild on
every save. Measured on 2026-10-23 with release builds on an AMD Ryzen 9 5900HX,
rustc 1.99.1, over real stdio LSP: a generated 1000-definition file (125 KB)
with three smaller documents open, and the same file with 100 definitions.

| Scenario | 100 defs before | after | 1000 defs before | after |
| --- | ---: | ---: | ---: | ---: |
| Open, to first diagnostics | 23 ms | 24 ms | 193 ms | 192 ms |
| Keystroke, to its own diagnostics | 22 ms | 3 ms | 206 ms | 42 ms |
| Keystroke, until every document settled | 173 ms | 150 ms | 378 ms | 151 ms |
| Ten keystrokes typed in a burst | 324 ms | 151 ms | 2.33 s | 155 ms |
| Save, four documents open | 30 ms | 0.1 ms | 353 ms | 0.4 ms |
| Column completion, a new text | 37 ms | 17 ms | 345 ms | 162 ms |
| Column completion, the same text again | 31 ms | 0.4 ms | 343 ms | 2.7 ms |

A settled column of 150 ms is the publish debounce, not work: the small file
has nothing left to do once that timer fires.

The burst and save figures were fixed in the server: superseded keystrokes are
collapsed before they are analysed, and the compilation cache is keyed by root
instead of being rebuilt. Completion was fixed by probing the live database
rather than a rebuilt copy of it.

The keystroke figures came a different way, in the compiler: type checking is
memoized per definition (`def_check`), so an edit re-checks the definition it
touched and the checks that read it, not the file. In process, the same edit to
a generated file takes about 1.6 ms and 23 ms including parsing.

What is still linear in the file: parsing (about 8 ms per 1000 definitions, per
keystroke), the aggregation that collects a module's per-definition results, and
compilation, which walks the edited module's definitions once. A keystroke that
changes a *type* re-checks more than its direct readers, because each check
reads the module's name map; narrowing that is the next step.

## A keystroke that moves the file

The figures above were measured with edits that keep every later source position
fixed — a same-length replacement, or an edit in the last definition. Typing does
not work that way: a character inserted anywhere but the end of a file shifts
every byte below it, and the incremental cache used to read that shift as an edit
to every definition it touched. A definition below the cursor had *different
spans*, and spans were part of both the per-definition facts that authorize reuse
and the parse trees the checker is handed. One inserted character therefore
re-checked and re-elaborated the rest of the file.

Measured on 2026-10-26 with release builds on an AMD Ryzen 9 5900HX, rustc
1.99.1, over real stdio LSP: one generated file, one document open, and a single
character inserted per keystroke at the top of the file, so every definition
moves. Median of eight keystrokes, each timed from `textDocument/didChange` to
the `publishDiagnostics` the server sends for that document.

| File | Before | After |
| --- | ---: | ---: |
| 200 definitions, 16 KB | 29.5 ms | 2.5 ms |
| 1000 definitions, 84 KB | 154.3 ms | 18.8 ms |

An insertion in the middle of the file measured the same as one at the top
(140.4 ms before, 18.6 ms after), because it moves just as much of the file. Open,
to first diagnostics, is unchanged — 164 ms for the 1000-definition file — since
that is a cold compile and nothing about it is incremental. What is left per
keystroke at 1000 definitions is parsing (about 5 ms), the per-definition facts
walk salsa re-runs because the file's parse changed (about 3.7 ms), the module
check's aggregation (about 5 ms), and the reuse itself (about 0.3 ms).

Both binaries were run alternately, and the absolute figures move with the
machine: a second set of runs on a busier machine measured the same pair at
180 ms and 32 ms. The ratio is what the change is about.

Cold compilation of a catalogue pays a little for this. Compiling a generated
100 000-definition file once, end to end (`cagara large.cagara`), went from
2.73 s to 2.85 s — about 4% — because the workspace now keeps each definition's
source text beside its input (to tell an untouched definition from a moved one
without re-parsing anything) and the module check reads a definition's offset
once per definition. Peak memory for the 100 000-definition benchmark went from
852 MB to 875 MB. That is the trade: a catalogue is compiled once, and typed in
continuously.

Two changes, one in each half of the pipeline, and neither adds a second route:

- **The check is kept across a move.** `Workspace::sync_defs` decides whether a
  definition's input is still current by comparing its *text*, not its parsed
  tree. Its check is memoized per definition, so a definition whose own text did
  not change keeps its check even when it moved. The spans that check recorded
  are then stale by the distance it moved — one offset for the whole definition —
  and `module_check` applies it where spans become positions: rendered
  diagnostics, the module's typed-span index, and the two places in
  normalization that build an origin from a typed span.
- **The compilation is reused across a move.** `DefinitionFacts` no longer
  includes a definition's offset, so a definition that only moved is not dirty,
  and its cached relation is reused with every `Rel::At` translated through the
  definition that wrote it (`incremental::Moves`). The translation is refused
  rather than guessed when the module's definition list changed.

Both are still keyed on exact facts, with no hash standing in for equality: the
text of a definition and the closed data its check produced. What changed is what
counts as part of a definition — an offset does not.
