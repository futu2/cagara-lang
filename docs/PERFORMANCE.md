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
clone. Returned checks and compilations are dropped before editing; a client
retaining older results would use additional memory. Source generation, edit
string construction, test assertions, and result destruction are outside the
listed phase timings. Workspace destruction is reported separately by the
harness. Linux `VmHWM` records process peak resident memory, including the
harness, across both cold compilation and editing.

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
