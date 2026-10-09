# Cagara for VS Code

Syntax highlighting for `.cagara` files, plus diagnostics, hover types,
go-to-definition, references, highlights, outline, and name / column
completion from the language server, `cagara lsp`.

## Install from a release

Each [GitHub release](https://github.com/futu2/cagara-lang/releases) has one
VSIX per platform, each carrying the `cagara` server built for that platform:

```
cagara-vscode-v0.2.0-linux-x64.vsix
cagara-vscode-v0.2.0-linux-arm64.vsix
cagara-vscode-v0.2.0-darwin-x64.vsix
cagara-vscode-v0.2.0-darwin-arm64.vsix
cagara-vscode-v0.2.0-win32-x64.vsix
cagara-vscode-v0.2.0-win32-arm64.vsix
```

Install the one matching your machine:

```sh
code --install-extension cagara-vscode-v0.2.0-linux-x64.vsix
```

VS Code picks the package for the platform it is running on. Nothing needs to
be installed or on `PATH`: the extension runs the server it ships. Releases
also have `cagara-<tag>-<platform>.tar.gz` static binaries for using `cagara`
from a shell, and a target-less `cagara-vscode-v0.2.0.vsix` for platforms with
no package of their own — that one carries no server, so it needs `cagara` on
`PATH` or a build in the workspace.

## Where the server comes from

The extension runs `cagara lsp`, taking the first of:

1. the `cagara.path` setting;
2. the server bundled in the extension, in `bin/<target>/`;
3. `target/release/cagara`, then `target/debug/cagara`, in an open trusted
   workspace folder or in any directory above it;
4. `cagara` on `PATH`.

A released VSIX fills `bin/` for you. The extension development host does not:
F5 runs this source folder rather than a VSIX, so its launch task stages the
local build into `bin/<target>/` first and F5 then takes the same path a
release does. Step 3 is the fallback for a package that carries no server, and
searches upward, so a build in the repository root serves a nested workspace
such as `examples/`.

## Setup from source

Build `cagara` from the repository root:

```sh
cargo build --release
```

A build in an untrusted workspace is ignored, so opening a checkout cannot
execute a binary it contains; set `cagara.path` to choose one explicitly.

## Develop

```sh
cd editors/vscode
npm install
npm run compile
npm test
```

Open this folder in VS Code and press F5 (Run Extension) to launch it on
`examples/`. The launch task compiles this folder and stages the repository's
`target/release/cagara` into `bin/<target>/`, so F5 runs the same bundled-server
path a released VSIX does. Build the server first:

```sh
cargo build --release    # from the repository root
```

Re-run `npm run stage:server` (or relaunch) after rebuilding: a staged server
outranks a workspace build, so the extension keeps using the staged copy until
it is refreshed, and the stage step warns when it is older than the sources.
Run **Cagara: Restart Language Server** to pick up a new server without
relaunching.

`npm test` compiles `src/` and runs the `node:test` suites in `src/test/`: the
search for the language server is covered there rather than by hand, since a
lookup that is only ever tried against a packaged VSIX is how it came to miss
the build sitting above an opened `examples/`.

Packaging expects a server per VS Code target under `bin/`, for example
`bin/linux-x64/cagara`; `bin/` is gitignored, and the release workflow fills
it from the binaries it builds. `npm run package -- --target linux-x64` packs
a platform-specific VSIX, which `code --install-extension` installs. A
target-less `npm run package` carries no server by design, so remove `bin/`
(or package with `--target`) if you want a build of that shape locally.
