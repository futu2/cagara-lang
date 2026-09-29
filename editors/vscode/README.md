# Cagara for VS Code

Syntax highlighting for `.cagara` files, plus diagnostics, hover types,
go-to-definition, references, highlights, outline, and name / column
completion from `cagara-lsp`.

## Setup

Build the server from the repository root:

```sh
cargo build --release -p cagara-lsp
```

The extension looks for the server in this order:

1. the `cagara.server.path` setting;
2. `target/release/cagara-lsp`, then `target/debug/cagara-lsp`, in an open
   workspace folder;
3. `cagara-lsp` on `PATH`.

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
