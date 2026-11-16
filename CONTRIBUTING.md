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

The SQL differential tests in `crates/cagara-sql/tests/engines.rs` compare rows
against real engines. An engine that is not installed is skipped by default; set
`CAGARA_REQUIRE_ENGINES=1` to turn a missing engine into a failure, which is what
a full local run does. [`docs/ENGINE-TESTS.md`](docs/ENGINE-TESTS.md) has the
command and the setup for each engine — SQLite, DuckDB, PostgreSQL through
PGlite, Trino and MariaDB — and for the sqlglot emulation of the rest.

CI does not run these: it installs no engines and sets `CAGARA_NO_ENGINES=1`, so
every execution test skips while the SQL-text checks (and the rest of the suite)
still run. That is deliberate — the comparison needs three downloads and two
servers on every push, and the runner's own `sqlite3` is older than the 3.46 that
`floor` needs, so omitting the engine would fail rather than skip. See "CI does
not run these" in `docs/ENGINE-TESTS.md`.

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
