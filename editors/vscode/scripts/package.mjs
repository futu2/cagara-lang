#!/usr/bin/env node
// `npm run package` -- stage the compiler, then hand off to `vsce package`.
//
// The staging step exists because a bundled server outranks every other
// candidate the extension considers: `cagara.path`, a workspace build and PATH
// all lose to `bin/<target>/cagara`. A VSIX is installed on machines that have
// no checkout, so a stale or missing server in it cannot be corrected from the
// outside -- the extension simply runs the wrong compiler, and the symptom is
// bogus diagnostics rather than anything saying "stale build".
//
// Which is why this is a wrapper rather than a `prepackage` script: staging is
// only correct when packaging *for a target*.
//
//   npm run package -- --target linux-x64   stages bin/linux-x64/, then packs
//   npm run package                         packs target-less, no server
//
// A target-less VSIX carries no server **by design** (it is the fallback for
// platforms no release builds for, and it relies on PATH or a workspace
// build). Staging there would quietly change what that artifact is, so the
// check is on the `--target` flag rather than on the platform we happen to
// run on: cross-packaging for another platform must stage that platform's
// server, not this machine's.

import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const extensionDir = resolve(here, "..");

// `npm` on Windows is a `.cmd` file, which `spawnSync` refuses to run unless a
// shell is involved. Everything else this script spawns is a plain executable
// and is run directly.
const npm = "npm";

const args = process.argv.slice(2);
const targetFlag = args.findIndex((a) => a === "--target" || a.startsWith("--target="));
const target = targetFlag === -1 ? undefined : args[targetFlag].split("=")[1] ?? args[targetFlag + 1];

if (target) {
  console.log(`cagara: packaging for ${target}; staging the compiler first`);

  // Compile before staging. `stage-server.mjs` imports `vscodeTarget` and
  // `rustTargetFor` from the compiled `out/serverpath.js`, so it reads
  // whatever that directory happens to contain. `vsce` does run
  // `vscode:prepublish` -> `tsc`, but only after staging, and that ordering
  // is what made `--target` fail with a bare "rustTargetFor is not a
  // function": the source had the new export and `out/` did not. Run the same
  // compile script here so staging always sees current output.
  const compiled = spawnSync(npm, ["run", "compile"], {
    cwd: extensionDir,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (compiled.status !== 0) {
    console.error("cagara: not packaging; the extension did not compile");
    process.exit(compiled.status ?? 1);
  }

  const staged = spawnSync(process.execPath, [join(here, "stage-server.mjs")], {
    cwd: extensionDir,
    stdio: "inherit",
    env: {
      ...process.env,
      // Refuse to ship a compiler that predates its sources. The staging
      // script checks for that and only warns by default, because F5 wants
      // to run whatever was built last.
      CAGARA_STAGE_STRICT: "1",
      // The target being packaged, which is not necessarily this machine's.
      CAGARA_STAGE_TARGET: target,
    },
  });
  if (staged.status !== 0) {
    console.error("cagara: not packaging; the compiler could not be staged");
    process.exit(staged.status ?? 1);
  }
} else if (args.length === 0) {
  console.log(
    "cagara: packaging without --target, so no server is bundled.\n" +
      "cagara: pass `-- --target <platform>-<arch>` for a VSIX that carries one.",
  );
}

const vsce = join(extensionDir, "node_modules", ".bin", "vsce");
const packed = spawnSync(vsce, ["package", "--skip-license", ...args], {
  cwd: extensionDir,
  stdio: "inherit",
});
process.exit(packed.status ?? 1);
