// Reading `resource`-scoped settings for a document in a two-folder workspace.
//
// The extension host cannot be driven from `npm test`, so the scoping decision
// lives in `settings.ts` and is tested here. The fake answers the window value
// when it is asked without a scope — which is what the old call sites did — so
// a lookup that drops the URI fails these tests instead of passing by accident.

import assert from "node:assert/strict";
import { test } from "node:test";

import { compileSettingsFor, copyOnSaveFor, showSqlOnSaveFor } from "../settings";
import type { ConfigurationSource } from "../settings";

type Values = Record<string, unknown>;

/**
 * A workspace whose window says one thing and whose folders say another.
 *
 * Folder A compiles for Postgres, folder B for MySQL; the window default is
 * `ansi` and copies nothing. A read that forgets its scope gets the window
 * values, and the assertions below are written so that shows up as a failure.
 */
function twoFolderWorkspace(): ConfigurationSource<string> {
  const window: Values = {
    dialect: "ansi",
    extraArgs: "",
    copyOnSave: false,
    showSqlOnSave: false,
  };
  const folders: Record<string, Values> = {
    "file:///repo/a": {
      dialect: "postgres",
      extraArgs: "--pretty --optimize",
      copyOnSave: true,
      showSqlOnSave: true,
    },
    "file:///repo/b": {
      dialect: "mysql",
      extraArgs: "",
      copyOnSave: false,
      showSqlOnSave: false,
    },
  };
  return {
    getConfiguration(section, scope) {
      assert.equal(section, "cagara");
      // An unscoped read is the window value: that is exactly what the bug
      // did, so a lookup that drops the URI sees `ansi` and fails.
      const values = scope === undefined ? window : (folders[scope] ?? window);
      return {
        get<T>(key: string, defaultValue: T): T {
          const value = values[key];
          return value === undefined ? defaultValue : (value as T);
        },
      };
    },
  };
}

test("each folder's document compiles with its own folder's dialect", () => {
  const source = twoFolderWorkspace();
  assert.deepEqual(compileSettingsFor(source, "file:///repo/a"), {
    dialect: "postgres",
    extraArgs: ["--pretty", "--optimize"],
  });
  assert.deepEqual(compileSettingsFor(source, "file:///repo/b"), {
    dialect: "mysql",
    extraArgs: [],
  });
});

test("copyOnSave and showSqlOnSave are read per folder", () => {
  const source = twoFolderWorkspace();
  assert.equal(copyOnSaveFor(source, "file:///repo/a"), true);
  assert.equal(copyOnSaveFor(source, "file:///repo/b"), false);
  assert.equal(showSqlOnSaveFor(source, "file:///repo/a"), true);
  assert.equal(showSqlOnSaveFor(source, "file:///repo/b"), false);
});

test("a document outside the workspace folders gets the window values", () => {
  // An untitled or single-file window has no folder to scope to, and the
  // window's settings are what VS Code returns for it.
  const source = twoFolderWorkspace();
  assert.deepEqual(compileSettingsFor(source, "untitled:Untitled-1"), {
    dialect: "ansi",
    extraArgs: [],
  });
  assert.equal(copyOnSaveFor(source, "untitled:Untitled-1"), false);
});
