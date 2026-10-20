# Cagara

[![CI](https://github.com/futu2/cagara-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/futu2/cagara-lang/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/futu2/cagara-lang?include_prereleases&sort=semver)](https://github.com/futu2/cagara-lang/releases)
[![Downloads](https://img.shields.io/github/downloads/futu2/cagara-lang/total)](https://github.com/futu2/cagara-lang/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-linux%20amd64%20%7C%20arm64-lightgrey?logo=linux)](https://github.com/futu2/cagara-lang/releases)
[![VS Code](https://img.shields.io/badge/VS%20Code-extension-007ACC?logo=visualstudiocode)](editors/vscode)

A small typed query language that compiles to SQL. Write queries as
pipelines, and the type checker catches mistakes like missing columns,
ungrouped fields in an aggregate, or `null` misuse before any SQL runs.

```cagara
orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"

main = orders
  & where (.status == "paid")
  & agg { user_id = group .user_id, revenue = sum .amount, n = count }
  & where (.n >= 5)
  & order [desc (coalesce 0.0 .revenue)]
```

`cagara your-file.cagara` compiles `main` — the file's entry point — to:

```sql
SELECT user_id, revenue, n
FROM (SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n
      FROM public.orders
      WHERE (status = 'paid')
      GROUP BY user_id) AS t1
WHERE (n >= 5)
ORDER BY COALESCE(revenue, 0.0) DESC NULLS LAST;
```

The repository's [`examples/report.cagara`](examples/report.cagara) uses its
aggregate report as `main`; inspect one of its other named queries with, for
example, `cagara examples/report.cagara --only adults`.

## Features

- Pipelines with `&`: `where`, `select`, `agg`, `order`, `limit`, joins, window functions.
- Filtered aggregates: `sumFilter p .amount`, `countFilter p` and friends compile to portable `CASE` equivalents of SQL's `FILTER (WHERE ...)`.
- Hindley–Milner type inference with extensible records, so column types flow through the pipeline.
- Explicit nulls: the right side of a left join and aggregates like `sum` are `maybe`.
- Multi-way conditionals: `case { cond => value; ...; true => default }`, which compiles to one searched `CASE`.
- Modules via `import "file.cagara"`, plus a standard library written in Cagara (`prelude.cagara`).
- ANSI SQL by default, with dialect support and test coverage described in [`docs/SQL-DIALECTS.md`](docs/SQL-DIALECTS.md).
- A file's entry point is the definition named `main`, and that is the one statement `cagara` prints; a file without one is a library and prints nothing.
- A formatter (`cagara fmt`) and a language server (`cagara lsp`) with diagnostics, hover, go-to-definition and completion.

Cagara is intentionally a query language rather than a general-purpose
language: its types are built in, and users cannot declare new types.

## Install

The [VS Code extension](editors/vscode) bundles the language server, so
installing it needs no separate binary and nothing on `PATH` — download the
VSIX for your platform from
[Releases](https://github.com/futu2/cagara-lang/releases) and
`code --install-extension` it.

For the command line, download a static binary for Linux (amd64 / arm64),
macOS, or Windows from
[Releases](https://github.com/futu2/cagara-lang/releases), or build from
source:

```sh
cargo build --release    # -> target/release/cagara
```

## Usage

```sh
cagara file.cagara [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
cagara -              # read the source from stdin
cagara fmt [--check] <files...>
cagara lsp
cagara --version
```

`cagara fmt` normalizes line endings to LF and preserves a leading UTF-8 BOM.

## Learn more

- [A language guide](docs/LEARN.md), [nullability reference](docs/NULLABILITY.md),
  and [row type reference](docs/ROW-TYPES.md)
- [`CAGARA.md`](CAGARA.md): a single self-contained reference written for code-generating agents;
  paste it into an agent's context before asking for Cagara code
- [`examples/`](examples): filtering, aggregation, windows, joins, modules, error messages
- [`docs/PLAN.md`](docs/PLAN.md): architecture and current gaps
- Editor support: [VS Code](editors/vscode) · [Neovim](editors/nvim)

## License

[MIT](LICENSE)
