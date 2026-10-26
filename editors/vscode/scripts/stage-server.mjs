#!/usr/bin/env node
// Stages the local `cagara` build into bin/<target>/, where the extension looks
// for a bundled server. F5 runs the extension from source rather than from a
// VSIX, so without this it has no bundled server and searches the workspace
// instead; staging makes the development host exercise the same path a
// released VSIX does.
//
// Run by the "cagara: stage server" preLaunchTask (after `npm run compile`,
// since the target name comes from the compiled module) and by
// `npm run stage:server`. It builds the server before copying it: a stage is a
// copy, and a copy is a snapshot, so without the build nothing in the loop
// rebuilds and the editor keeps launching whatever was staged last time.

import { spawnSync } from "node:child_process";
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
let rustTargetFor;
try {
  ({ vscodeTarget, rustTargetFor } = require(join(extensionDir, "out", "serverpath.js")));
} catch {
  console.error(
    `cagara: cannot load ${join(extensionDir, "out", "serverpath.js")}.\n` +
      `Run \`npm run compile\` in ${extensionDir} first.`,
  );
  process.exit(1);
}

// A `require` that succeeds proves only that the file parses; it says nothing
// about the exports being the current ones. Compiling `serverpath.ts` and not
// staging it is how `--target` came to fail with a bare "rustTargetFor is not
// a function" from a line further down: the old `out/` loaded fine and simply
// did not have the new export. Name the stale output here instead.
if (typeof vscodeTarget !== "function" || typeof rustTargetFor !== "function") {
  const missing = [
    typeof vscodeTarget === "function" ? undefined : "vscodeTarget",
    typeof rustTargetFor === "function" ? undefined : "rustTargetFor",
  ].filter(Boolean);
  console.error(
    `cagara: ${join(extensionDir, "out", "serverpath.js")} is out of date:\n` +
      `  missing export${missing.length === 1 ? "" : "s"}: ${missing.join(", ")}\n` +
      `Its source has changed since it was last compiled. Run \`npm run compile\`\n` +
      `in ${extensionDir} and try again.`,
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
const hostTarget = vscodeTarget(process.platform, process.arch);
const hostExe = process.platform === "win32" ? "cagara.exe" : "cagara";
// A plain `cargo build` writes to target/<profile>/, whatever toolchain it used,
// so a musl-built host binary is still there. Packaging for this machine can
// therefore use the plain path; packaging for another one has to name the
// triple, which is what a release build for that platform produced.
const native = join(repoRoot, "target", profile, hostExe);
const rustTarget = process.env.CAGARA_STAGE_RUST_TARGET?.trim() || rustTargetFor(target);
const triple = rustTarget ? join(repoRoot, "target", rustTarget, profile, exe) : undefined;

const forcedRust = process.env.CAGARA_STAGE_RUST_TARGET?.trim();

// Build the server first, so the copy below cannot be a snapshot of a build
// from last week. Two cases are skipped, and for opposite reasons:
//
//   - packaging for another platform: the binary for that VSIX was built on
//     that platform (the release workflow downloads it), and building here
//     would produce the wrong one;
//   - `CAGARA_STAGE_RUST_TARGET`, where the caller asked for a specific
//     toolchain's output and this script must not second-guess it.
//
// `cargo build` is the whole point: nothing else in the loop rebuilds, so an
// editor launched by F5 ran whatever was staged last, and a copied file keeps
// the copy's timestamp and so looks current however old it is.
const crossTarget = Boolean(forcedTarget) && forcedTarget !== hostTarget;
if (process.env.CAGARA_STAGE_NO_BUILD === "1") {
  console.log("cagara: CAGARA_STAGE_NO_BUILD=1, staging the existing build");
} else if (crossTarget) {
  console.log(`cagara: packaging for ${target}; not building it on this machine`);
} else if (forcedRust) {
  console.log(`cagara: CAGARA_STAGE_RUST_TARGET=${forcedRust}; not building, staging it`);
} else {
  const build = ["build", "-p", "cagara-cli"];
  if (profile === "release") {
    build.push("--release");
  } else {
    build.push("--profile", profile);
  }
  console.log(`cagara: cargo ${build.join(" ")}`);
  const built = spawnSync("cargo", build, { cwd: repoRoot, stdio: "inherit" });
  if (built.error) {
    console.error(`cagara: could not run cargo (${built.error.message}); not staging.`);
    process.exit(1);
  }
  if (built.status !== 0) {
    console.error("cagara: the build failed; not staging a binary from before it.");
    process.exit(built.status ?? 1);
  }
}

// Which build to stage: of the two paths that can hold one, the newer.
//
// `cargo build --release` writes target/release/, while a build for a named
// triple -- what a release uses, and what packaging for another platform needs
// -- writes target/<triple>/release/. Preferring the triple whenever it existed
// staged that one however old it was, so a fresh local build was ignored. The
// triple is still the only correct answer when the target is not this
// machine's, which `crossTarget` already covers by leaving only it to choose.
let source;
let shadowedTarget = false;
const candidates = [];
if (triple && existsSync(triple)) {
  candidates.push(triple);
}
if (target === hostTarget && existsSync(native)) {
  candidates.push(native);
}
if (candidates.length > 0) {
  candidates.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
  source = candidates[0];
  // Naming an explicit rust target means the caller expects that toolchain's
  // output; staging a different binary instead should not pass silently.
  shadowedTarget = Boolean(forcedRust) && source !== triple;
} else {
  // Neither exists: name the triple path, so the error below names the file a
  // `cargo build --target <triple>` would create.
  source = triple ?? native;
}

// Set by `npm run package`. F5 wants a staged server even from a build that is
// a few seconds behind the sources -- you are iterating. A VSIX is different:
// it is copied around, installed elsewhere, and its bundled server outranks
// every other candidate, so shipping one that predates the sources bakes a
// compiler bug into the extension with nothing on the user's machine able to
// correct it. That is worth refusing rather than warning about.
const strict = process.env.CAGARA_STAGE_STRICT === "1";

if (!existsSync(source)) {
  // Name the command that actually writes to the missing path. A plain
  // `cargo build --release` fills target/release/, so telling someone to run it
  // when the path it wanted was target/<triple>/release/ sends them in circles.
  const tripleBuild = rustTarget && source !== native
    ? `  cargo build ${profile === "release" ? "--release " : ""}--target ${rustTarget}`
    : `  cargo build${profile === "release" ? " --release" : ""}`;
  console.error(
    `cagara: no ${profile} build at ${source}.\n` +
      (rustTarget
        ? `The ${target} target is built by the ${rustTarget} toolchain.\n`
        : "") +
      (strict
        ? `Refusing to package a VSIX with no compiler in it. Build one first, in\n` +
          `${repoRoot}:\n` +
          `${tripleBuild.replace(/`/g, "")}\n` +
          `(the Rust target has to be added first: rustup target add ${rustTarget ?? "<triple>"})`
        : `Run \`${tripleBuild.trim()}\` in ${repoRoot}, then launch again.`),
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
if (shadowedTarget) {
  console.warn(
    `cagara: warning: CAGARA_STAGE_RUST_TARGET asked for ${rustTarget}, but no build\n` +
      `exists there; staging this machine's ${profile} build instead. If the VSIX is\n` +
      `meant to run elsewhere, build it first:\n` +
      `  cargo build${profile === "release" ? " --release" : ""} --target ${rustTarget}`,
  );
}
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
