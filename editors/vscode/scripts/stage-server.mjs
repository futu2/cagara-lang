#!/usr/bin/env node
// Stages the local `cagara` build into bin/<target>/, where the extension looks
// for a bundled server. F5 runs the extension from source rather than from a
// VSIX, so without this it has no bundled server and searches the workspace
// instead; staging makes the development host exercise the same path a
// released VSIX does.
//
// Run by the "cagara: stage server" preLaunchTask (which compiles first, since
// the target name comes from the compiled module) and by `npm run stage:server`.

import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const extensionDir = resolve(here, "..");
const repoRoot = resolve(extensionDir, "..", "..");

// The same function the extension uses, so the directory staged here is the
// one the extension looks in. Requires `npm run compile` to have run.
let vscodeTarget;
try {
  ({ vscodeTarget } = require(join(extensionDir, "out", "serverpath.js")));
} catch {
  console.error(
    `cagara: cannot load ${join(extensionDir, "out", "serverpath.js")}.\n` +
      `Run \`npm run compile\` in ${extensionDir} first.`,
  );
  process.exit(1);
}

/**
 * The most recently modified file the server is built from, or undefined.
 * Used only to warn about a build that predates the sources.
 *
 * `tests/` is skipped because a test never reaches the binary: editing one
 * left the guard insisting the build was stale while cargo reported it up to
 * date, so a package that was in fact current could not be produced. An
 * `examples/` or `benches/` directory would be the same. What remains is the
 * source that is compiled into the server, plus the prelude, which is
 * `include_str!`d into it.
 */
function newestSource(root) {
  let newest;
  const consider = (file) => {
    const mtimeMs = statSync(file).mtimeMs;
    if (!newest || mtimeMs > newest.mtimeMs) {
      newest = { path: file, mtimeMs };
    }
  };
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (
        entry.name === "target" ||
        entry.name === "node_modules" ||
        entry.name === "tests" ||
        entry.name.startsWith(".")
      ) {
        continue;
      }
      const candidate = join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(candidate);
      } else if (entry.name.endsWith(".rs") || entry.name === "Cargo.toml") {
        consider(candidate);
      }
    }
  };
  const crates = join(root, "crates");
  if (existsSync(crates)) {
    walk(crates);
  }
  for (const file of ["prelude.cagara", "Cargo.toml"]) {
    const candidate = join(root, file);
    if (existsSync(candidate)) {
      consider(candidate);
    }
  }
  return newest;
}

// `npm run package -- --target <t>` sets this, because cross-packaging has to
// fill the directory for the target being packaged rather than the one this
// machine happens to be. F5 and `npm run stage:server` leave it unset and get
// this machine's target.
const forcedTarget = process.env.CAGARA_STAGE_TARGET?.trim();
const target = forcedTarget || vscodeTarget(process.platform, process.arch);
if (!target) {
  // Nothing is built for this machine, so leave bin/ alone and let the
  // extension fall back to a workspace build or PATH.
  console.log(
    `cagara: no released server for ${process.platform}-${process.arch}; not staging ` +
      `(a workspace build or PATH is used instead).`,
  );
  process.exit(0);
}

// The executable name follows the *target*, not the host: packaging a Windows
// VSIX from Linux has to produce `cagara.exe`.
const exe = target.startsWith("win32") || process.env.CAGARA_STAGE_EXE === "cagara.exe"
  ? "cagara.exe"
  : "cagara";
const profile = process.env.CAGARA_STAGE_PROFILE ?? "release";
const source = join(repoRoot, "target", profile, exe);

// Set by `npm run package`. F5 wants a staged server even from a build that is
// a few seconds behind the sources -- you are iterating. A VSIX is different:
// it is copied around, installed elsewhere, and its bundled server outranks
// every other candidate, so shipping one that predates the sources bakes a
// compiler bug into the extension with nothing on the user's machine able to
// correct it. That is worth refusing rather than warning about.
const strict = process.env.CAGARA_STAGE_STRICT === "1";

if (!existsSync(source)) {
  console.error(
    `cagara: no ${profile} build at ${source}.\n` +
      (strict
        ? `Refusing to package a VSIX with no compiler in it. Build one first:\n` +
          `  cargo build --release`
        : `Run \`cargo build --release\` in ${repoRoot}, then launch again.`),
  );
  process.exit(1);
}

// Checked before staging, so a refusal leaves the previous staging intact
// rather than replacing a good copy with an old one.
const built = statSync(source).mtimeMs;
const newest = newestSource(repoRoot);
const stale = newest && newest.mtimeMs > built;
if (stale && strict) {
  console.error(
    `cagara: the ${profile} build at ${source} is older than ${newest.path}.\n` +
      `Refusing to package a VSIX with a stale compiler: the bundled server\n` +
      `outranks every other candidate, so the extension would run the older\n` +
      `one and no setting on the user's machine could correct it.\n` +
      `Run \`cargo build --release\` and package again.`,
  );
  process.exit(1);
}

const destDir = join(extensionDir, "bin", target);
const dest = join(destDir, exe);
mkdirSync(destDir, { recursive: true });
// Copied rather than symlinked or pointed at in place: the extension treats a
// bundled server as self-contained, which is what a release VSIX ships.
copyFileSync(source, dest);
if (process.platform !== "win32") {
  chmodSync(dest, 0o755);
}
console.log(`cagara: staged ${source} -> ${dest}`);

// A staged server outranks a build in the workspace, so a `cargo build` after
// this point would be shadowed silently. Say so at launch instead.
if (stale) {
  console.warn(
    `cagara: warning: the staged ${profile} build is older than ${newest.path}.\n` +
      `Run \`cargo build --release\` to rebuild, or the extension keeps running the older server.`,
  );
}
