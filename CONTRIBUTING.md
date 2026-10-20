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

The SQL differential tests use the `sqlite3` and `duckdb` command-line shells.
They skip a missing engine by default; set `CAGARA_REQUIRE_ENGINES=1` to make a
missing engine fail instead. CI requires both.

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
