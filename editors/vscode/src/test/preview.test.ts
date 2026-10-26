// What the SQL preview is called and how wide the split it opens in should be.
//
// The rules are small but the failure they prevent is not: an even split puts a
// read-only three-line SELECT beside the file being written at half the width
// each, on every save, and a layout command that fires when it should not moves
// the user's own window around.

import assert from "node:assert/strict";
import { test } from "node:test";

import { narrowSplitLayout, splitLayout, sqlPreviewPath } from "../preview";

test("a preview is named after the file it came from", () => {
  assert.equal(sqlPreviewPath("/w/q.cagara"), "/w/q.cagara.sql");
});

test("two files get two previews, one file gets one", () => {
  // The point of the path: a tab per file, so the second save refreshes the
  // tab the first one made instead of leaving it behind.
  assert.equal(sqlPreviewPath("/w/a.cagara"), sqlPreviewPath("/w/a.cagara"));
  assert.notEqual(sqlPreviewPath("/w/a.cagara"), sqlPreviewPath("/w/b.cagara"));
});

test("the SQL gets a third, the source the rest", () => {
  const layout = narrowSplitLayout();
  assert.equal(layout.orientation, 0, "columns side by side");
  assert.equal(layout.groups.length, 2);
  const [source, sql] = layout.groups.map((g) => g.size);
  assert.ok(source > sql, `source ${source} should outrank sql ${sql}`);
  assert.equal(sql / (source + sql), 1 / 3);
});

test("a split this extension makes is sized", () => {
  assert.deepEqual(splitLayout(1, 2, false), narrowSplitLayout());
});

test("a split that already existed is left alone", () => {
  // Two groups before the save means the SQL opened into one of them; setting
  // the layout would re-lay out a workbench the user had arranged.
  assert.equal(splitLayout(2, 2, false), undefined);
  assert.equal(splitLayout(3, 3, false), undefined);
});

test("no split, no sizing", () => {
  // The preview tab was already open, so nothing was created.
  assert.equal(splitLayout(2, 2, false), undefined);
  assert.equal(splitLayout(1, 1, false), undefined);
});

test("widths are set once, so a dragged divider survives", () => {
  assert.equal(splitLayout(1, 2, true), undefined);
});
