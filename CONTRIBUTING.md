# Contributing

Cagara is a Rust workspace. The compiler pipeline is documented in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the language reference is
[`CAGARA.md`](CAGARA.md).

## Before sending a change

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
```

Install the runner once — CI does the same thing with `taiki-e/install-action`:

```sh
cargo install cargo-nextest --locked     # or: cargo binstall cargo-nextest
```

### Why nextest, and only nextest

`cargo test` runs the workspace's 19 test *binaries* one after another, so a single
30-second binary holds the run while the other fifteen cores idle: 103 s in total.
`cargo nextest run --workspace` runs each test in its own process and the binaries
concurrently, and takes **47 s** for the same 817 tests. It also reports each
failure with its own captured output instead of interleaving everything.

One runner is the point. A local run and a CI run should mean the same thing, and
two runners drift: a test that passes under one and fails under the other is a test
whose result you cannot read.

```sh
cargo nextest run --workspace          # everything
cargo nextest run -p cagara-sql        # one package
cargo nextest run --test engines       # one integration binary
cargo nextest run -E 'binary(fuzz)'    # filter expressions, not just substrings
cargo nextest run --no-fail-fast       # every failure, not just the first
```

`.config/nextest.toml` caps two test groups. The cap is a *measured throughput
tuning*, not only a safety valve: the full workspace takes 62 s at `lsp = 2`, 56 s
at 4, and 116 s at 8, because eight language-server processes leave nothing for the
other fourteen binaries. The language-server tests also wait on a wall-clock window
for the server to publish, so uncapped they fail for scheduling reasons.

### The one gap: doctests

nextest deliberately does not compile doctests — an example in a doc comment is a
program, not a test, and nextest's model is one process per test. This workspace has
none, so `cargo test --workspace --doc` is a no-op today; CI runs it anyway so that
the first person to write one finds out there. Locally:

```sh
cargo nextest run --workspace && cargo test --workspace --doc
```

### If a test is slow

**Split it.** A test that loops over 2500 inputs is a single thread no matter which
runner you use or how many cores you have, because both `cargo test` and `cargo
nextest` parallelise *across* tests and not inside one. Every slow test here has
been split by its natural index — the seed range in `cagara-sql`, the source file in
`cagara-fmt`, the chain length in `cagara-hir` — and each split took its test from
tens of seconds to a few. That is where nearly all the speed in this suite came
from; the runner only decides which test waits for which.

**Do not assert on wall-clock time.** A budget that is generous on an idle machine
can be too short under load, and then the test measures the runner rather than the
code. Two ways to avoid it, both used here:

- `crates/cagara-sql/tests/perf.rs` measures **CPU time** (`getrusage`), which does
  not care how many other processes exist — a descheduled process accumulates none.
- `crates/cagara-lsp` waits on a **generous** budget for the server to answer (a
  hang detector, in seconds) and a short one only for the assertions that expect
  silence, where the wait is the point.

The SQL differential tests in `crates/cagara-sql/tests/engines.rs` compare rows
against real engines. An engine that is not installed is skipped by default; set
`CAGARA_REQUIRE_ENGINES=1` to turn a missing engine into a failure, which is what
a full local run does. [`docs/ENGINE-TESTS.md`](docs/ENGINE-TESTS.md) has the
command and the setup for each engine — SQLite, DuckDB, PostgreSQL through
PGlite, Trino and MariaDB — and for the sqlglot emulation of the rest.

CI runs the SQLite and DuckDB half of these on every push, at versions the workflow
pins itself and checks against a SHA-256 (`SQLITE_VERSION` / `DUCKDB_VERSION` in
`.github/workflows/ci.yml`). That job installs the two engines and runs

```sh
CAGARA_ENGINES=sqlite,duckdb CAGARA_REQUIRE_ENGINES=1 \
  cargo nextest run -p cagara-sql --test engines --locked
```

The other three engines still need downloads and servers, so the `rust` job stays
engine-free and sets `CAGARA_NO_ENGINES=1`; the full five-engine matrix is the local
run above. See "What CI runs" in `docs/ENGINE-TESTS.md`.

The pipeline snapshot is updated deliberately:

```sh
UPDATE_SNAPSHOTS=1 cargo nextest run -p cagara-hir --test pipeline_golden
CAGARA_BLESS=1 cargo nextest run -p cagara-sql --test corpus
```

Read the diff before committing it. A snapshot change is a change in compiler
output and needs an explanation in the commit message.

`cagara-cli`'s `doc_blocks` tests compile the examples in `CAGARA.md` and the
README, and check that the versions in the repository agree. Keep those in step
when changing an example or bumping the version.

## Style

User-visible prose in diagnostics is part of the interface. Prefer an actionable
message, point at the smallest useful span, and add a regression test under the
crate that owns the check. Do not add source-level operator declarations: the
operator table is fixed and lives in `crates/cagara-syntax/src/ops.rs`.
