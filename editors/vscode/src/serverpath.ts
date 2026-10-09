// Where the `cagara` language server to run comes from.
//
// This deliberately imports no `vscode`, so the search order can be tested
// directly. A lookup that is only ever exercised against a packaged VSIX is
// how the extension came to fail under F5 with `target/release/cagara`
// sitting in the repository root.

import * as fs from "fs";
import * as path from "path";

/** Where a resolved server was found. */
export type ServerSource = "configured" | "bundled" | "workspace" | "path";

export interface ResolvedServer {
  /** The command to run. A bare name is resolved through `PATH` when spawned. */
  command: string;
  source: ServerSource;
}

export interface ServerSearch {
  /** The `cagara.path` setting, if it is set. */
  configured?: string;
  /** The server bundled in the extension, if this build carries one. */
  bundled?: string;
  /** An untrusted workspace is never searched for a repository-local build. */
  trusted: boolean;
  /** Workspace folder paths, in order. */
  folders: string[];
  /** The executable name for this platform. */
  exe: string;
  /** Injected by tests; defaults to `fs.existsSync`. */
  exists?: (candidate: string) => boolean;
}

/** `dir`, then each ancestor of it, nearest first, ending at the filesystem root. */
export function selfAndAncestors(dir: string): string[] {
  const dirs: string[] = [];
  let current = path.resolve(dir);
  for (;;) {
    dirs.push(current);
    const parent = path.dirname(current);
    if (parent === current) {
      return dirs;
    }
    current = parent;
  }
}

/**
 * Paths that would be accepted as the server, best first: the extension's
 * bundled copy, then `target/release` or `target/debug` in a workspace folder
 * or any ancestor of it.
 *
 * The bare executable name is absent on purpose — `PATH` resolves it at
 * spawn, so it is never stat-ed.
 */
export function serverCandidates(search: ServerSearch): string[] {
  const candidates: string[] = [];
  if (search.bundled) {
    candidates.push(search.bundled);
  }
  if (search.trusted) {
    for (const folder of search.folders) {
      for (const dir of selfAndAncestors(folder)) {
        for (const profile of ["release", "debug"]) {
          candidates.push(path.join(dir, "target", profile, search.exe));
        }
      }
    }
  }
  return [...new Set(candidates)];
}

/**
 * The VS Code target name for a platform and architecture: the value
 * `vsce package --target` takes, and the `bin/<target>/` directory a release
 * stages the server in. `undefined` for a platform and architecture no release
 * builds for (`ia32`, `armhf`), which then falls through to a workspace build
 * and PATH rather than running a binary built for a different CPU.
 */
export function vscodeTarget(platform: string, arch: string): string | undefined {
  if (arch !== "x64" && arch !== "arm64") {
    return undefined;
  }
  return platform === "linux" || platform === "darwin" || platform === "win32"
    ? `${platform}-${arch}`
    : undefined;
}

/**
 * The server to run: the configured path, else the bundled one, else a
 * `target/` build in the workspace or above it, else the executable name for
 * `PATH` to resolve.
 *
 * Searching upward is what makes an extension development host work: F5 opens
 * `examples/`, which has no `target/` of its own, while the build the
 * developer just made sits in the repository root above it.
 */
export function resolveServer(search: ServerSearch): ResolvedServer {
  const configured = search.configured?.trim();
  if (configured) {
    return { command: configured, source: "configured" };
  }
  const exists = search.exists ?? fs.existsSync;
  for (const candidate of serverCandidates(search)) {
    if (exists(candidate)) {
      return {
        command: candidate,
        source: candidate === search.bundled ? "bundled" : "workspace",
      };
    }
  }
  return { command: search.exe, source: "path" };
}
