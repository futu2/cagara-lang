// A PostgreSQL for the differential tests, without a server.
//
// PGlite is Postgres compiled to WebAssembly; this script is the shell the test
// drives the same way it drives `sqlite3`. The input is a *batch*: a setup
// script, then one or more queries, separated by a record separator (0x1e, which
// no SQL the compiler emits contains). The setup runs once, on a fresh in-memory
// database, and then each query runs against it and prints its rows one per line
// with columns joined by `|` and `NULL` for a null — sqlite3's `-list
// -nullvalue NULL` output. Result sets are separated by the same 0x1e, so the
// whole batch is one process instead of one per query: booting a WASM Postgres
// takes a few seconds, and a suite of twenty queries would pay it twenty times.
//
// Usage: node pglite.mjs < batch   (stdin: setup \x1e query \x1e query…)
//        node pglite.mjs --check   (exit 0 if PGlite is importable, 3 if not)
//
// Values print the way the other shells print them: booleans as `true` (the
// Rust side folds that to 1), dates as `YYYY-MM-DD`, arrays as `{1,2}` — and
// numbers as JavaScript renders them, which the comparison treats as equal to
// `99.0`.
import { readFileSync } from "node:fs";

const SEP = "\u001e";
let PGlite;
try {
  ({ PGlite } = await import("@electric-sql/pglite"));
} catch {
  process.exit(process.argv.includes("--check") ? 3 : 4);
}
if (process.argv.includes("--check")) process.exit(0);

function isoDate(d) {
  const iso = d.toISOString();
  return iso.endsWith("T00:00:00.000Z") ? iso.slice(0, 10) : iso.slice(0, 19).replace("T", " ");
}

function maybeDate(s) {
  const m = /^(\d{4}-\d\d-\d\d)T00:00:00(?:\.\d+)?Z$/.exec(s);
  if (m) return m[1];
  const t = /^(\d{4}-\d\d-\d\d)T(\d\d:\d\d:\d\d)(?:\.\d+)?Z$/.exec(s);
  return t ? `${t[1]} ${t[2]}` : s;
}

const fmt = (v) => {
  if (v === null || v === undefined) return "NULL";
  if (v instanceof Date) return isoDate(v);
  if (Array.isArray(v)) return "{" + v.map(fmt).join(",") + "}";
  if (typeof v === "string") return maybeDate(v);
  return String(v);
};

const [setup, ...queries] = readFileSync(0, "utf8").split(SEP);
const db = new PGlite();
try {
  await db.exec(setup);
} catch (e) {
  console.error(String(e?.message ?? e));
  process.exit(1);
}
const out = [];
for (const [i, q] of queries.entries()) {
  if (i > 0) out.push(SEP);
  try {
    const r = await db.query(q);
    for (const row of r.rows) out.push(Object.values(row).map(fmt).join("|"));
  } catch (e) {
    console.error(String(e?.message ?? e));
    process.exit(1);
  }
}
process.stdout.write(out.join("\n") + "\n");
// PGlite leaves a WASM runtime alive; the batch is done, so exit rather than
// wait for handles the test is not watching.
process.exit(0);
