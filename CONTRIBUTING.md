# Contributing

Cagara is a Rust workspace. The compiler pipeline is documented in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the language reference is
[`CAGARA.md`](CAGARA.md).

## Before sending a change

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
```

`cargo test --workspace` is what CI runs and what you get for free. It takes about
110 s, and most of that is *not* the tests: `cargo test` runs the workspace's 19
test *binaries* one after another, so a single 30-second binary holds the run while
the other fifteen cores idle. Two ways to fix that, both ~60 s:

**`cargo nextest`** — better output and per-test isolation, and the one this
workspace is configured for (`.config/nextest.toml`). 54 s:

```sh
cargo nextest run --workspace          # what `cargo test --workspace` does, faster
cargo nextest run -p cagara-sql        # one package
cargo nextest run -E 'binary(fuzz)'    # filter expressions, not just substrings
cargo nextest run --no-fail-fast       # see every failure, not just the first
```

Two differences worth knowing. It **does not run doctests**, so the habit is
`cargo nextest run --workspace && cargo test --workspace --doc` — this workspace
has no doctests today, so that second command is a no-op safeguard rather than a
chore. And it runs each test in **its own process**, which is why
`crates/cagara-lsp`'s suite takes 35 s alone but the whole workspace still finishes
in 54 s. `.config/nextest.toml` caps the language-server and engine tests because
they are the two groups whose timing depends on the machine — the cap is a measured
throughput tuning, not only a safety valve (see the comment in that file).

Installing it needs write access to `~/.cargo`, which a sandbox may not have. The
workaround used here:

```sh
CARGO_HOME=$PWD/.cargo-home cargo install cargo-nextest --locked --root $PWD/.nextest
export PATH=$PWD/.nextest/bin:$PATH
```

**`scripts/parallel-test.sh`** — the same idea in 40 lines with no dependency, if
you would rather not install anything. It runs several binaries at once inside a
*total* budget of one thread per core (64 s at the defaults; `JOBS` and
`TEST_THREADS` override it).

Two things to know if you add a slow test:

- **Split it.** A test that loops over 2500 inputs is a single thread no matter
  which runner you use or how many cores you have, because both `cargo test` and
  `cargo nextest` parallelise *across* tests and not inside one. Every slow test
  here has been split by its natural index — the seed range in `cagara-sql`, the
  source file in `cagara-fmt`, the chain length in `cagara-hir` — and each split
  took its test from tens of seconds to a few. That is where nearly all of the
  speed in this suite came from; the runners only decide who waits.
- **Do not assert on wall-clock time.** A budget that is generous on an idle
  machine can be too short under load, and then a test measures the runner rather
  than the code. Two ways to avoid it, both used here:
  - `crates/cagara-sql/tests/perf.rs` measures **CPU time** (`getrusage`), which
    does not care how many other processes exist — a descheduled process is simply
    not accumulating any.
  - `crates/cagara-lsp` waits on a **generous** budget for the server to answer
    (a hang detector, in seconds) and a short one only for the assertions that
    expect silence, where the wait is the point.

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
  cargo test -p cagara-sql --test engines --locked
```

The other three engines still need downloads and servers, so the `rust` job stays
engine-free and sets `CAGARA_NO_ENGINES=1`; the full five-engine matrix is the local
run above. See "What CI runs" in `docs/ENGINE-TESTS.md`.

The pipeline snapshot is updated deliberately:

```sh
UPDATE_SNAPSHOTS=1 cargo test -p cagara-hir --test pipeline_golden
CAGARA_BLESS=1 cargo test -p cagara-sql --test corpus
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
