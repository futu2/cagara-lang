// Argument building and output handling for "compile the active file to SQL".
//
// The two things worth testing here are the two that only bite in a real
// editor: whether `--dialect` reaches the command line, and what happens on a
// run that succeeds while printing nothing.

import assert from "node:assert/strict";
import { test } from "node:test";

import { DIALECTS, classify, compileArgs, splitArgs, stripBanner } from "../compile";

test("the dialect reaches the command line", () => {
  assert.deepEqual(compileArgs({ file: "q.cagara", dialect: "mysql" }), [
    "--dialect",
    "mysql",
    "q.cagara",
  ]);
});

test("no dialect flag when the setting is empty", () => {
  // The compiler has its own default; passing `--dialect ""` is a usage error.
  assert.deepEqual(compileArgs({ file: "q.cagara", dialect: "  " }), ["q.cagara"]);
});

test("no `--only`, so a file without main is not an error", () => {
  // `--only main` on a library would report "no query definition named
  // `main`" -- an error for a file that is not wrong. The default entry point
  // is what the compiler already does.
  const args = compileArgs({ file: "lib.cagara", dialect: "ansi" });
  assert.ok(!args.includes("--only"), `unexpected --only: ${args}`);
});

test("extra arguments are passed through, before the file", () => {
  const args = compileArgs({
    file: "q.cagara",
    dialect: "ansi",
    extraArgs: ["--pretty"],
  });
  assert.deepEqual(args, ["--dialect", "ansi", "--pretty", "q.cagara"]);
});

test("extra arguments split on whitespace, ignoring blanks", () => {
  assert.deepEqual(splitArgs("  --pretty   --optimize "), ["--pretty", "--optimize"]);
  assert.deepEqual(splitArgs(""), []);
  assert.deepEqual(splitArgs(undefined), []);
});

test("a successful run with SQL is SQL", () => {
  const outcome = classify("-- main\nSELECT a FROM s.t;\n", "", 0);
  assert.deepEqual(outcome, { kind: "sql", sql: "-- main\nSELECT a FROM s.t;" });
});

test("a successful run with no output is `empty`, not a failure", () => {
  // A library exits 0 with nothing to print. Reporting that as a failure would
  // pop an error every time a helper module is saved.
  assert.deepEqual(classify("", "", 0), { kind: "empty" });
  assert.deepEqual(classify("   \n", "", 0), { kind: "empty" });
});

test("a non-zero exit is a failure carrying the diagnostic", () => {
  const outcome = classify("", "q.cagara:1:2: error: unknown name `x`\n", 1);
  assert.equal(outcome.kind, "failed");
  assert.match(outcome.kind === "failed" ? outcome.message : "", /unknown name/);
});

test("a failure with nothing on stderr still says something", () => {
  const outcome = classify("", "", 101);
  assert.deepEqual(outcome, { kind: "failed", message: "cagara exited with code 101" });
});

test("the banner is stripped, because it is not SQL", () => {
  // Pasting `-- main` into a client that does not accept `--` fails.
  assert.equal(stripBanner("-- main\nSELECT a FROM s.t;\n"), "SELECT a FROM s.t;");
});

test("only a leading banner is stripped", () => {
  // A `--` inside the statement is a comment belonging to it.
  const sql = "SELECT a -- keep me\nFROM s.t;";
  assert.equal(stripBanner(sql), sql);
});

test("text with no banner is left alone", () => {
  assert.equal(stripBanner("SELECT 1;"), "SELECT 1;");
});

test("the dialect list is non-empty, unique, and starts at the default", () => {
  assert.equal(DIALECTS[0], "ansi");
  assert.equal(new Set(DIALECTS).size, DIALECTS.length);
  assert.ok(DIALECTS.includes("sqlite"));
});
