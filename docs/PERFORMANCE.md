# Catalog scale and incremental compilation

The cache preserves useful work across edits without putting mutable state in
the checked relational tree. It shares immutable module checks and name maps,
borrows imported schemes, and reuses erased queries only when their exact
checked facts and dependencies are unchanged. Inference still runs per module.

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
type (`use_tys.insert((m, id), self.zonk(&t))`) where it previously chased one
variable chain and stopped (`self.resolve(&t)`). That is the whole diff in
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
  an iterative dependency graph. Hash-based identities do not authorize reuse.
- Only successful unaffected query results are reused. Moved definitions
  are rebuilt so diagnostic and `Rel::At` locations refer to the current text.
  The one-query edit benchmark deliberately keeps later source positions fixed.
- A cold load still parses and checks the entire imported catalog. Editing
  the catalog itself still reparses and rechecks that module. The root layout
  also walks all root definitions and returns all query results after an edit.
  One re-elaboration therefore does not imply constant-time compilation.
- The import measurements use a namespace alias. An unaliased import merges
  exported names into the importing module's scope, which remains proportional
  to the number of exported names when that scope is rebuilt.
- Field completion uses a speculative `Workspace::snapshot` that rebuilds
  the source graph. These measurements do not establish fast field completion
  over a million-table catalog. Large symbol lists and error-heavy files are
  also outside this benchmark.

For large generated catalogs, keeping definitions in imported modules allows
small query edits to use the existing cache effectively. Supporting frequent
edits inside a million-definition module or cheap speculative completion would
require finer-grained parsing/inference or shared speculative snapshots; neither
is a property of the current implementation.
