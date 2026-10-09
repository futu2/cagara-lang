// The search order for the language server, including the layout that F5
// creates: the development host opens `examples/`, and the build is in the
// repository root above it.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import assert from "node:assert/strict";
import { test } from "node:test";

import { resolveServer, selfAndAncestors, vscodeTarget, type ServerSearch } from "../serverpath";

/** A real `target/` tree under a throwaway root, so `existsSync` is exercised. */
function tree(files: string[]): { root: string; dispose: () => void } {
  const root = mkdtempSync(join(tmpdir(), "cagara-search-"));
  for (const file of files) {
    mkdirSync(join(root, file, ".."), { recursive: true });
    writeFileSync(join(root, file), "");
  }
  return { root, dispose: () => rmSync(root, { recursive: true, force: true }) };
}

function search(overrides: Partial<ServerSearch> = {}): ServerSearch {
  return { trusted: true, folders: [], exe: "cagara", ...overrides };
}

test("finds the repository build above the opened examples folder", (t) => {
  // `cargo build --release` in the repository root, F5 opening `examples/`.
  const repo = tree(["target/release/cagara", "examples/report.cagara"]);
  t.after(repo.dispose);
  const resolved = resolveServer(search({ folders: [join(repo.root, "examples")] }));
  assert.equal(resolved.command, join(repo.root, "target/release/cagara"));
  assert.equal(resolved.source, "workspace");
});

test("finds an ancestor build from a deeply nested folder", (t) => {
  const repo = tree(["target/release/cagara", "examples/deep/nest/here"]);
  t.after(repo.dispose);
  const resolved = resolveServer(search({ folders: [join(repo.root, "examples/deep/nest/here")] }));
  assert.equal(resolved.command, join(repo.root, "target/release/cagara"));
});

test("prefers a workspace build over a debug one, and the nearer build", (t) => {
  const repo = tree(["target/release/cagara", "target/debug/cagara", "nested/target/debug/cagara"]);
  t.after(repo.dispose);
  // Nearest directory first, so the nested debug build beats nothing above it.
  const nested = resolveServer(search({ folders: [join(repo.root, "nested")] }));
  assert.equal(nested.command, join(repo.root, "nested/target/debug/cagara"));
  const root = resolveServer(search({ folders: [repo.root] }));
  assert.equal(root.command, join(repo.root, "target/release/cagara"));
});

test("a configured path wins over every other candidate", (t) => {
  const repo = tree(["target/release/cagara"]);
  t.after(repo.dispose);
  const resolved = resolveServer(
    search({
      configured: " /opt/cagara/bin/cagara ",
      bundled: join(repo.root, "bundled/cagara"),
      folders: [repo.root],
    }),
  );
  assert.deepEqual(resolved, { command: "/opt/cagara/bin/cagara", source: "configured" });
});

test("a bundled server wins over a workspace build", (t) => {
  const repo = tree(["target/release/cagara", "bundled/cagara"]);
  t.after(repo.dispose);
  const resolved = resolveServer(
    search({ bundled: join(repo.root, "bundled/cagara"), folders: [repo.root] }),
  );
  assert.equal(resolved.command, join(repo.root, "bundled/cagara"));
  assert.equal(resolved.source, "bundled");
});

test("an untrusted workspace is not searched for a build", (t) => {
  const repo = tree(["target/release/cagara"]);
  t.after(repo.dispose);
  // Trust gates only the repository-local probe; `PATH` still resolves.
  const resolved = resolveServer(search({ trusted: false, folders: [repo.root] }));
  assert.deepEqual(resolved, { command: "cagara", source: "path" });
});

test("falls back to PATH when nothing is found", () => {
  assert.deepEqual(resolveServer(search()), { command: "cagara", source: "path" });
  assert.deepEqual(resolveServer(search({ folders: ["/nonexistent-workspace"] })), {
    command: "cagara",
    source: "path",
  });
});

test("uses the platform executable name", (t) => {
  const repo = tree(["target/release/cagara.exe"]);
  t.after(repo.dispose);
  const resolved = resolveServer(search({ folders: [repo.root], exe: "cagara.exe" }));
  assert.equal(resolved.command, join(repo.root, "target/release/cagara.exe"));
});

test("selfAndAncestors walks up to the root, nearest first", () => {
  const dirs = selfAndAncestors("/a/b/c");
  assert.deepEqual(dirs.slice(0, 3), ["/a/b/c", "/a/b", "/a"]);
  assert.equal(dirs[dirs.length - 1], "/");
});

test("vscodeTarget names the directories a release stages servers in", () => {
  // These strings must match `vsce package --target`, since the release stages
  // the server in bin/<target>/ and the extension looks there.
  assert.equal(vscodeTarget("linux", "x64"), "linux-x64");
  assert.equal(vscodeTarget("linux", "arm64"), "linux-arm64");
  assert.equal(vscodeTarget("darwin", "x64"), "darwin-x64");
  assert.equal(vscodeTarget("darwin", "arm64"), "darwin-arm64");
  assert.equal(vscodeTarget("win32", "x64"), "win32-x64");
  assert.equal(vscodeTarget("win32", "arm64"), "win32-arm64");
});

test("vscodeTarget has no answer for architectures no release builds", () => {
  // Better to miss and fall through to PATH than to run a binary built for a
  // different CPU.
  assert.equal(vscodeTarget("linux", "ia32"), undefined);
  assert.equal(vscodeTarget("linux", "arm"), undefined);
  assert.equal(vscodeTarget("freebsd", "x64"), undefined);
});
