# Cagara for VS Code

Syntax highlighting for `.cagara` files, plus diagnostics, hover types,
go-to-definition, references, highlights, outline, and name / column
completion from the language server, `cagara lsp`.

## Install from a release

Each [GitHub release](https://github.com/futu2/cagara-lang/releases) has one
VSIX per platform, each carrying the `cagara` server built for that platform:

```
cagara-vscode-v0.3.1-linux-x64.vsix
cagara-vscode-v0.3.1-linux-arm64.vsix
cagara-vscode-v0.3.1-darwin-x64.vsix
cagara-vscode-v0.3.1-darwin-arm64.vsix
cagara-vscode-v0.3.1-win32-x64.vsix
cagara-vscode-v0.3.1-win32-arm64.vsix
```

Install the one matching your machine:

```sh
code --install-extension cagara-vscode-v0.3.1-linux-x64.vsix
```

VS Code picks the package for the platform it is running on. Nothing needs to
be installed or on `PATH`: the extension runs the server it ships. Releases
also have `cagara-<tag>-<platform>.tar.gz` static binaries for using `cagara`
from a shell, and a target-less `cagara-vscode-v0.3.1.vsix` for platforms with
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

Staging builds before it copies — `cargo build -p cagara-cli --release`, skipped
when packaging for another platform — because a stage is a copy and a copy is a
snapshot: nothing else in the loop rebuilds, so F5 would otherwise launch
whatever was staged last. The server also names itself, as the first line in
the *Cagara Language Server* output channel:

```
cagara 0.3.1 (24b8547, release, x86_64-unknown-linux-gnu, built 2026-10-26 03:12 UTC)
```

That line is the only reliable answer to "which compiler is this window
running": a staged or copied binary cannot be told from a current one by its
path, and a copy carries the copy's timestamp rather than the build's.

## Formatting

`Shift+Alt+F` formats the file with `cagara fmt`. The server sends back only the
lines that changed, so the rest of the file keeps its tokens, folds and cursor,
and `cagara.formatOnSave` can do the same on every save:

| Setting | Default | Effect |
|---|---|---|
| `cagara.formatOnSave` | `false` | Format `.cagara` files as they are saved, before the text is written. |

On Linux `Ctrl+Shift+I` is also Format Document, and in the **extension
development host** that key opens Developer Tools instead: both commands claim
it, and the DevTools binding exists only in a development host, where its
weight beats the format binding's. Someone formatting from the keyboard while
developing the extension therefore opens a debugger, which looks like the
editor freezing for a minute while DevTools starts. Use `Shift+Alt+F` there.
A normal window — an installed VSIX — has no DevTools binding on that key, so
`Ctrl+Shift+I` formats as usual.

## Compiling to SQL

The extension can run `cagara <file>` for you and put the result on the
clipboard, so SQL does not have to be fetched from a terminal.

| Setting | Default | Effect |
|---|---|---|
| `cagara.copyOnSave` | `false` | Compile and copy on every save of a `.cagara` file. |
| `cagara.dialect` | `ansi` | Dialect to compile to. The names are the ones `cagara --dialect` accepts. |
| `cagara.showSqlOnSave` | `false` | Also show the SQL beside the source. Off, saving only fills the clipboard. |
| `cagara.sqlPreviewFocus` | `false` | Put the keyboard in the SQL as it appears. Off, saving does not interrupt. |
| `cagara.extraArgs` | `""` | Extra `cagara` arguments, split on whitespace, e.g. `--pretty` or `--optimize`. |

**Cagara: Copy SQL** in the command palette does the same for the active file
and always shows the SQL, since running it is a request to see the result.
Both paths use the compiler the extension already runs as a language server,
so `cagara.path` applies.

The SQL appears in a preview tab rather than an editor of its own, because the
point is to glance at it while saving:

- **It does not take the keyboard.** With `cagara.sqlPreviewFocus` off — the
  default — the cursor stays in the file being saved, and the status bar says
  the SQL went beside it. Turn the setting on to put the cursor in the SQL.
- **It takes a third of the window, not half.** The first time the extension
  makes the split it sizes the two columns 2:1 and remembers that it did, so a
  divider you drag stays where you put it. A window that already had two
  columns is left as it is.
- **It wraps.** `Cagara SQL` is a language of its own, wrapping at the pane's
  width, so a statement stays readable in a third of the screen. Override it
  with `"[cagara-sql]": { "editor.wordWrap": "..." }` in `settings.json`; your
  `[sql]` settings apply to the SQL you write by hand, not to this preview.
- **One tab per file**, named after the source (`q.cagara.sql`), refreshed in
  place by the next save instead of leaving a trail of `Untitled-1` behind.
  It is read-only, and its highlighting is the built-in SQL grammar's.

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

`npm run compile` type-checks and compiles `src/` with `tsc`, then bundles it
into `dist/extension.js` with esbuild; `dist/extension.js` is `main`, so F5 runs
the same file the VSIX ships. `out/` still exists, because `npm test` and
`npm run stage:server` read it, but it is not what the extension loads. The
bundle is why `node_modules/` is not in the package: 223 of the 235 files in a
VSIX were the language client's own dependency tree, and folding it into one
file leaves eight files to install. A source map is written beside the bundle
so a breakpoint in `src/` still binds under F5; `.vscodeignore` keeps it out of
the VSIX. Editing during F5 wants `npm run bundle:watch` alongside
`npm run watch`, since a stale bundle is a window running code you did not
write.

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

