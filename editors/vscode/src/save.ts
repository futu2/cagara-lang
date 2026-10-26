// The two decisions a save makes before it writes a file: whether to format,
// and with what options.
//
// Both are functions of values read elsewhere, so the extension host does the
// lookups and these stay testable in plain node — the same shape as
// `compile.ts` and `serverpath.ts`.

/**
 * Whether a save of this document should be formatted first.
 *
 * The setting is the user's and the language id is the extension's; neither is
 * consulted here. Formatting a `.cagara` file is only ever asked for by a
 * `.cagara` file, so the language id is checked rather than trusted: a save
 * handler registered by this extension still sees every document that happens
 * to be saved while it is running.
 */
export function shouldFormat(formatOnSave: boolean, languageId: string): boolean {
  return formatOnSave && languageId === "cagara";
}

/** What a formatting provider takes, structurally `vscode.FormattingOptions`. */
export interface FormatSettings {
  tabSize: number;
  insertSpaces: boolean;
}

/**
 * The options to hand the formatter, from the `editor.*` settings.
 *
 * A settings value can be anything — `tabSize` is a number in `settings.json`
 * and can be a string in a language-specific override that was typed by hand —
 * so each is checked rather than cast, and a wrong one falls back to the
 * editor's own defaults instead of reaching the provider as a string where a
 * number belongs.
 */
export function formattingOptions(
  read: (key: "tabSize" | "insertSpaces") => unknown,
): FormatSettings {
  const tabSize = read("tabSize");
  const insertSpaces = read("insertSpaces");
  return {
    tabSize: typeof tabSize === "number" ? tabSize : 2,
    insertSpaces: typeof insertSpaces === "boolean" ? insertSpaces : true,
  };
}
