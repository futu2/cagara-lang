// The decisions a save makes: which files are formatted, and with what options.

import assert from "node:assert/strict";
import { test } from "node:test";

import { formattingOptions, shouldFormat } from "../save";

test("only a cagara file with the setting on is formatted", () => {
  assert.equal(shouldFormat(true, "cagara"), true);
  assert.equal(shouldFormat(false, "cagara"), false);
  // The handler sees every document saved while the extension is active.
  assert.equal(shouldFormat(true, "sql"), false);
  assert.equal(shouldFormat(true, "markdown"), false);
});

test("formatting options come from the editor settings", () => {
  const settings: Record<string, unknown> = { tabSize: 4, insertSpaces: false };
  assert.deepEqual(formattingOptions((key) => settings[key]), {
    tabSize: 4,
    insertSpaces: false,
  });
});

test("a missing or mistyped setting falls back to the editor default", () => {
  assert.deepEqual(formattingOptions(() => undefined), { tabSize: 2, insertSpaces: true });
  // A hand-written language override can hold a string where a number belongs;
  // the provider gets a number either way.
  assert.deepEqual(formattingOptions((key) => (key === "tabSize" ? "4" : "yes")), {
    tabSize: 2,
    insertSpaces: true,
  });
});
