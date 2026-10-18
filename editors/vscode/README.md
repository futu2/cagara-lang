# Cagara for VS Code

Syntax highlighting for `.cagara` files, plus diagnostics, hover types,
go-to-definition, references, highlights, outline, and name / column
completion from the language server, `cagara lsp`.

## Install from a release

Each [GitHub release](https://github.com/futu2/cagara-lang/releases) has one
VSIX per platform, each carrying the `cagara` server built for that platform:

```
cagara-vscode-v0.3.0-linux-x64.vsix
cagara-vscode-v0.3.0-linux-arm64.vsix
cagara-vscode-v0.3.0-darwin-x64.vsix
cagara-vscode-v0.3.0-darwin-arm64.vsix
cagara-vscode-v0.3.0-win32-x64.vsix
cagara-vscode-v0.3.0-win32-arm64.vsix
```

Install the one matching your machine:

```sh
code --install-extension cagara-vscode-v0.3.0-linux-x64.vsix
```

VS Code picks the package for the platform it is running on. Nothing needs to
be installed or on `PATH`: the extension runs the server it ships. Releases
also have `cagara-<tag>-<platform>.tar.gz` static binaries for using `cagara`
from a shell, and a target-less `cagara-vscode-v0.3.0.vsix` for platforms with
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

## Compiling to SQL

The extension can run `cagara <file>` for you and put the result on the
clipboard, so SQL does not have to be fetched from a terminal.

| Setting | Default | Effect |
|---|---|---|
| `cagara.copyOnSave` | `false` | Compile and copy on every save of a `.cagara` file. |
| `cagara.dialect` | `ansi` | Dialect to compile to. The names are the ones `cagara --dialect` accepts. |
| `cagara.showSqlOnSave` | `false` | Also open the SQL beside the source. Off, saving only fills the clipboard. |
| `cagara.extraArgs` | `""` | Extra `cagara` arguments, split on whitespace, e.g. `--pretty` or `--optimize`. |

**Cagara: Copy SQL** in the command palette does the same for the active file
and always opens the SQL, since running it is a request to see the result.
Both paths use the compiler the extension already runs as a language server,
so `cagara.path` applies.

The SQL is compiled from the **file on disk**, which is why saving is the
natural trigger: the compiler reads the file, not the editor's buffer. Running
the command on a file with unsaved edits asks you to save first rather than
copying SQL that does not match what is on screen.

A file whose entry point is `main` produces one statement. A file with no
`main` is a library, so there is nothing to copy and the extension says so
instead of putting an empty string on the clipboard. A compile error opens a
notification with the diagnostic, which the language server also underlines in
the editor.

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
it from the binaries it builds.

`npm run package -- --target linux-x64` packs a platform-specific VSIX, which
`code --install-extension` installs. It stages the compiler itself before
packing, so there is no separate copy step and no way to forget one — and it
**refuses** a build that is older than the sources rather than shipping it:

```
cagara: the release build at target/release/cagara is older than
crates/cagara-syntax/src/parser.rs.
Refusing to package a VSIX with a stale compiler: ...
```

That refusal is deliberate. A bundled server outranks `cagara.path`, a
workspace build and `PATH` alike, so a VSIX carrying an old compiler makes
every user of it run that compiler, with nothing on their machine able to
override it — the symptom is bogus diagnostics, not anything that mentions a
stale build. Build first, then package:

```sh
cargo build --release
npm run package -- --target linux-x64
```

A target-less `npm run package` carries no server **by design**: it is the
fallback for platforms no release builds for, and it relies on `PATH` or a
workspace build. Staging would quietly turn it into a different artifact, so
it is left alone. `--target` also decides the directory and the executable
name, so a Windows VSIX can be packaged from Linux and still contain
`cagara.exe`.

