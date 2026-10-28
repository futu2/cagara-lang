// The decisions behind the SQL preview: what its document is, and how wide the
// split it opens in should be.
//
// No `vscode` import here, so both can be tested in plain node — the same
// shape as `compile.ts` and `save.ts`. The editor work stays in `extension.ts`.

/** The scheme the SQL preview documents are served under. */
export const SQL_PREVIEW_SCHEME = "cagara-sql";

/**
 * The language the preview is shown as.
 *
 * Its own language rather than `sql`, because the two want different editor
 * defaults: a read-only preview of generated SQL wants wrapping on, and a file
 * you are writing by hand wants whatever you set for SQL. Widening `[sql]`
 * would move the second to serve the first.
 */
export const SQL_PREVIEW_LANGUAGE = "cagara-sql";

/**
 * The path one file's preview is served at: the source's path, plus `.sql`.
 *
 * One document per source file, named after it. `openTextDocument({content})`
 * would be shorter but gives every preview the title `Untitled-1`, leaves the
 * previous one behind, and cannot be told apart from the next save's — so
 * saving twice piles up tabs that all look alike, and there is no saying which
 * file's SQL is in front of you.
 */
export function sqlPreviewPath(fsPath: string): string {
  return `${fsPath}.sql`;
}

/**
 * Whether a save's SQL has to be pushed into the preview document.
 *
 * A save that produced the same SQL as the last one has nothing to show: the
 * provider-backed document already holds that text, so firing the change event
 * makes the editor re-read and re-render a document that is already right. The
 * text is compared rather than the save counted because the interesting case is
 * exactly "saved again, nothing changed", which is most saves.
 */
export function previewNeedsRefresh(shown: string | undefined, next: string): boolean {
  return shown !== next;
}

/**
 * What `vscode.setEditorLayout` takes.
 *
 * Only the two fields used here: `orientation` 0 is columns side by side, and
 * a group's `size` is a relative weight.
 */
export interface EditorLayout {
  orientation: 0 | 1;
  groups: { size: number }[];
}

/**
 * The layout that leaves the SQL a third of the window.
 *
 * VS Code splits evenly, and an even split hands a read-only three-line
 * `SELECT` half the window while halving the file being written.
 */
export function narrowSplitLayout(): EditorLayout {
  return { orientation: 0, groups: [{ size: 2 }, { size: 1 }] };
}

/**
 * The layout to apply after showing the SQL, or undefined to leave the widths
 * alone.
 *
 * Sized once, and only when this extension is what split the window: VS Code
 * keeps the widths it is given, so later saves and later sessions inherit the
 * narrow split on their own, and a divider the user has dragged is theirs from
 * then on. A window that already held two groups is not touched — the SQL
 * opened into one of them and re-laying out the whole workbench to widen the
 * source would move files nobody asked to move.
 */
export function splitLayout(
  groupsBefore: number,
  groupsAfter: number,
  sized: boolean,
): EditorLayout | undefined {
  if (sized || groupsBefore !== 1 || groupsAfter !== 2) {
    return undefined;
  }
  return narrowSplitLayout();
}
