# Cagara

[![CI](https://github.com/futu2/cagara-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/futu2/cagara-lang/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/futu2/cagara-lang?include_prereleases&sort=semver)](https://github.com/futu2/cagara-lang/releases)
[![Downloads](https://img.shields.io/github/downloads/futu2/cagara-lang/total)](https://github.com/futu2/cagara-lang/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-linux%20amd64%20%7C%20arm64-lightgrey?logo=linux)](https://github.com/futu2/cagara-lang/releases)
[![VS Code](https://img.shields.io/badge/VS%20Code-extension-007ACC?logo=visualstudiocode)](editors/vscode)

A small typed functional language that compiles to SQL. Write queries as
pipelines, and the type checker catches mistakes like missing columns,
ungrouped fields in an aggregate, or `null` misuse before any SQL runs.

```haskell
orders : query { id = int, user_id = int, amount = float, status = string } =
  table "public" "orders"

revenue = orders
  & where (.status == "paid")
  & agg { user_id = group .user_id, revenue = sum .amount, n = count }
  & where (.n >= 5)
  & order [desc .revenue]
```

`cagara report.cagara --only revenue` compiles it to:

```sql
SELECT user_id, revenue, n
FROM (SELECT user_id, SUM(amount) AS revenue, COUNT(*) AS n
      FROM public.orders
      WHERE (status = 'paid')
      GROUP BY user_id) AS t1
WHERE (n >= 5)
ORDER BY revenue DESC NULLS LAST;
```

## Features

- Pipelines with `&`: `where`, `select`, `agg`, `order`, `limit`, joins, window functions.
- Hindley–Milner type inference with extensible records, so column types flow through the pipeline.
- Explicit nulls: the right side of a left join and aggregates like `sum` are `maybe`.
- Modules via `import "file.cagara"`, plus a standard library written in Cagara (`prelude.cagara`).
- ANSI SQL by default, or any [sqlglot](https://github.com/tobymao/sqlglot) dialect via `--dialect postgres|mysql|...`.
- A formatter (`cagara fmt`) and a language server (`cagara lsp`) with diagnostics, hover, go-to-definition and completion.

## Install

Download a static Linux binary (amd64 / arm64) and the VS Code extension from
[Releases](https://github.com/futu2/cagara-lang/releases), or build from source:

```sh
cargo build --release    # -> target/release/cagara
```

## Usage

```sh
cagara file.cagara [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
cagara fmt [--check] <files...>
cagara lsp
```

## Learn more

- [`examples/`](examples): filtering, aggregation, windows, joins, modules, error messages
- [`docs/PLAN.md`](docs/PLAN.md): design and architecture
- Editor support: [VS Code](editors/vscode) · [Neovim](editors/nvim)

## License

[MIT](LICENSE)
