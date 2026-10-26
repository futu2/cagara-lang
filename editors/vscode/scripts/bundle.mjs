#!/usr/bin/env node
// `npm run bundle` -- compile the extension host into one file, `dist/extension.js`.
//
// The extension host code is six files. The package was 235, because
// `vscode-languageclient` arrives with a dependency tree of its own —
// `vscode-jsonrpc`, `vscode-languageserver-protocol`, `vscode-languageserver-
// types`, `vscode-languageserver-textdocument`, `semver`, `minimatch`,
// `brace-expansion`, `balanced-match` — and every one of them is needed at
// runtime, so no ignore pattern can take them out. `vsce` says so on every
// package run. Bundling is the answer it asks for: the tree is folded into one
// file at build time, and `dependencies` is empty, so the VSIX carries the code
// it runs and nothing else. It installs and activates faster too — 235 files
// from a VSIX are 235 files the editor host stats and parses per window.
//
// `vscode` stays external: the editor provides it at runtime, and the module
// only exists inside the extension host, so bundling it would fail to resolve
// and shipping it would be shipping the wrong copy of the API.
//
// Not minified, and `keepNames` on: a stack trace out of the VSIX has no source
// map beside it — `.vscodeignore` drops `**/*.map` — so the shipped file is the
// one a trace has to be readable in. The map is still written, for F5: without
// it a breakpoint in `src/extension.ts` would not bind, because the extension
// host is running `dist/extension.js`.

import { build, context } from "esbuild";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const extensionDir = resolve(here, "..");

/** Shared by the one-shot build and the watcher. */
const options = {
  absWorkingDir: extensionDir,
  entryPoints: ["src/extension.ts"],
  outfile: "dist/extension.js",
  bundle: true,
  platform: "node",
  // The `main` field is CommonJS; `engines.vscode` 1.91 runs Node 20.
  format: "cjs",
  target: "node20",
  external: ["vscode"],
  keepNames: true,
  // For the debugger, not for the package: `.vscodeignore` keeps `**/*.map`
  // out of the VSIX.
  sourcemap: true,
  logLevel: "info",
};

if (process.argv.includes("--watch")) {
  const ctx = await context(options);
  await ctx.watch();
  console.log("cagara: watching src/ for the extension bundle");
} else {
  await build(options);
}
