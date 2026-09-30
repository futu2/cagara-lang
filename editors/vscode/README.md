# Cagara for VS Code

Syntax highlighting for `.cagara` files, plus diagnostics, hover types,
go-to-definition, references, highlights, outline, and name / column
completion from the language server, `cagara lsp`.

## Install from a release

Each [GitHub release](https://github.com/futu2/cagara-lang/releases) has
`cagara-<tag>-linux-{amd64,arm64}.tar.gz` (static binaries) and
`cagara-vscode-<tag>.vsix`:

```sh
tar -xzf cagara-v0.1.0-linux-amd64.tar.gz
install cagara-v0.1.0-linux-amd64/cagara ~/.local/bin/
code --install-extension cagara-vscode-v0.1.0.vsix
```

## Setup from source

Build `cagara` from the repository root:

```sh
cargo build --release
```

The extension runs `cagara lsp`, looking for `cagara` in this order:

1. the `cagara.path` setting;
2. `target/release/cagara`, then `target/debug/cagara`, in an open trusted
   workspace folder;
3. `cagara` on `PATH`.

## Develop

```sh
cd editors/vscode
npm install
npm run compile
```

Open this folder in VS Code and press F5 (Run Extension) to launch it on
`examples/`. `npm run package` builds a `.vsix`, which
`code --install-extension cagara-0.1.0.vsix` installs.

Run **Cagara: Restart Language Server** after rebuilding the server.
