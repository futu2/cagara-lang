// Starts the language server (`cagara lsp`, over stdio) for `.cagara` files,
// and can compile the active file to SQL and put it on the clipboard.

import { spawn } from "child_process";
import * as fs from "fs";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

import { classify, compileArgs, splitArgs, stripBanner } from "./compile";
import { SQL_PREVIEW_LANGUAGE, SQL_PREVIEW_SCHEME, splitLayout, sqlPreviewPath } from "./preview";
import { formattingOptions, shouldFormat } from "./save";
import { describeServer, resolveServer, vscodeTarget, type ResolvedServer } from "./serverpath";

let client: LanguageClient | undefined;

/**
 * The SQL shown in each preview tab, by document URI.
 *
 * The preview is served by a content provider rather than opened as an
 * untitled buffer, so that a file has one tab, named after it, which the next
 * save refreshes where it stands. This map is what the provider answers from,
 * and so also what a tab restored after a window reload reads: nothing, since
 * compiled SQL is not kept across a restart.
 */
const previewText = new Map<string, string>();

/** Fires when a preview's SQL is replaced, so an open tab re-reads the map. */
const previewChanged = new vscode.EventEmitter<vscode.Uri>();

/** `workspaceState` key: the SQL split has been sized once in this workspace. */
const PREVIEW_SIZED = "cagara.sqlPreviewSized";

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
    previewChanged,
    vscode.workspace.registerTextDocumentContentProvider(SQL_PREVIEW_SCHEME, {
      onDidChange: previewChanged.event,
      provideTextDocumentContent: (uri) => previewText.get(uri.toString()) ?? "",
    }),
    vscode.commands.registerCommand("cagara.restartServer", async () => {
      await stop();
      await start(context);
    }),
    vscode.commands.registerCommand("cagara.copySql", () =>
      copySql(context, "cagara.copySql"),
    ),
    vscode.workspace.onDidSaveTextDocument(async (doc) => {
      const config = vscode.workspace.getConfiguration("cagara");
      if (!config.get<boolean>("copyOnSave", false) || doc.languageId !== "cagara") {
        return;
      }
      await copySql(context, "cagara.copyOnSave", doc);
    }),
    // Formatting on save belongs in `onWillSave`, not `onDidSave`: the edits go
    // in with the write, so the file on disk is the formatted text and the
    // buffer is not left dirty the way formatting after the save leaves it.
    vscode.workspace.onWillSaveTextDocument((event) => {
      const config = vscode.workspace.getConfiguration("cagara", event.document.uri);
      if (!shouldFormat(config.get<boolean>("formatOnSave", false), event.document.languageId)) {
        return;
      }
      event.waitUntil(formatBeforeSave(event.document));
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("cagara.path")) {
        await stop();
        await start(context);
      }
    }),
  );
  await start(context);
}

/**
 * Compile a file and put its SQL on the clipboard.
 *
 * `showSqlOnSave` decides between the two useful behaviours: copy quietly so
 * the SQL can be pasted elsewhere, or show it beside the source to be read.
 * Both are offered because the two are wanted at different times, and copying
 * silently is the one that needs no window to be in the way.
 */
async function copySql(
  context: vscode.ExtensionContext,
  trigger: string,
  document?: vscode.TextDocument,
): Promise<void> {
  const doc = document ?? vscode.window.activeTextEditor?.document;
  if (!doc || doc.languageId !== "cagara") {
    void vscode.window.showWarningMessage("Cagara: no active .cagara file.");
    return;
  }
  // The compiler reads the file from disk, so unsaved edits would be compiled
  // stale. On save this cannot happen (the document is already written); from
  // the command it can, and saying so beats copying SQL that does not match
  // what is on screen.
  if (doc.isDirty) {
    void vscode.window.showWarningMessage(
      "Cagara: save the file first — the SQL is compiled from the file on disk.",
    );
    return;
  }
  const config = vscode.workspace.getConfiguration("cagara");
  const { command } = serverFor(context);
  const args = compileArgs({
    file: doc.uri.fsPath,
    dialect: config.get<string>("dialect", ""),
    extraArgs: splitArgs(config.get<string>("extraArgs", "")),
  });

  const run = await runCompiler(command, args);
  const outcome = classify(run.stdout, run.stderr, run.code);
  switch (outcome.kind) {
    case "sql": {
      const sql = stripBanner(outcome.sql);
      await vscode.env.clipboard.writeText(sql);
      // The manual command shows the SQL, because running it is a request to
      // see the result. On save that would be a pane per save, so it is
      // opt-in, and the clipboard stays the default so the SQL can be pasted
      // without anything moving.
      const show = trigger === "cagara.copySql" || config.get<boolean>("showSqlOnSave", false);
      if (show) {
        await showSql(context, sql, doc);
      } else {
        vscode.window.setStatusBarMessage("Cagara: SQL copied to clipboard", 3000);
      }
      break;
    }
    case "empty":
      // A file with no `main` is a library. Saying so is friendlier than a
      // silent no-op, but it is not an error and must not look like one.
      if (trigger === "cagara.copySql") {
        void vscode.window.showInformationMessage(
          "Cagara: this file has no `main` definition, so there is no SQL to copy.",
        );
      } else {
        vscode.window.setStatusBarMessage("Cagara: no `main` in this file", 3000);
      }
      break;
    case "failed":
      void vscode.window.showErrorMessage(`Cagara: ${firstLines(outcome.message, 6)}`);
      break;
  }
}

/**
 * Show the compiled SQL beside the file it came from.
 *
 * A preview, not a second editor. Three things make the difference, and each
 * one was a reason to close the pane again:
 *
 * - It does not take the keyboard. Saving is done in the file being written,
 *   and the next keystroke belongs there, not in a read-only document that
 *   swallowed it. `preserveFocus` keeps the cursor where the save left it.
 * - It is a third of the window, not half. VS Code splits evenly, which hands
 *   a three-line `SELECT` as much room as the source; see `splitLayout`.
 * - It wraps. Its language carries `editor.wordWrap: "on"` as a default, since
 *   a statement on one line is wider than any pane, and reading it by
 *   scrolling sideways is not reading it.
 */
async function showSql(
  context: vscode.ExtensionContext,
  sql: string,
  source: vscode.TextDocument,
): Promise<void> {
  const config = vscode.workspace.getConfiguration("cagara", source.uri);
  const focus = config.get<boolean>("sqlPreviewFocus", false);

  const uri = vscode.Uri.from({
    scheme: SQL_PREVIEW_SCHEME,
    path: sqlPreviewPath(source.uri.fsPath),
  });
  previewText.set(uri.toString(), sql);
  // A tab left over from an earlier save holds the old text: the document is
  // cached by URI, so the provider has to be told the content moved.
  previewChanged.fire(uri);

  // The language is set rather than inferred from the URI's `.sql` extension,
  // because that extension belongs to the built-in SQL language too, and which
  // of the two claims the document would depend on registration order. A
  // failure here costs the wrapping, not the SQL.
  const opened = await vscode.workspace.openTextDocument(uri);
  const document = await vscode.languages
    .setTextDocumentLanguage(opened, SQL_PREVIEW_LANGUAGE)
    .then(undefined, () => opened);

  // One preview per window: shown where the last one is, so a second file's
  // SQL does not open a third column, and the split is made at most once.
  const groupsBefore = vscode.window.tabGroups.all.length;
  await vscode.window.showTextDocument(document, {
    preview: true,
    preserveFocus: !focus,
    viewColumn: previewColumn() ?? vscode.ViewColumn.Beside,
  });

  const layout = splitLayout(
    groupsBefore,
    vscode.window.tabGroups.all.length,
    context.workspaceState.get<boolean>(PREVIEW_SIZED, false),
  );
  if (layout) {
    // Best effort: `vscode.setEditorLayout` is a command rather than an API,
    // and an editor width is not worth failing a save over.
    await vscode.commands
      .executeCommand("vscode.setEditorLayout", layout)
      .then(undefined, () => undefined);
    await context.workspaceState.update(PREVIEW_SIZED, true);
  }
  if (!focus) {
    // Nothing moved, so say where the SQL went: the status bar is the only
    // sign, and it does not take the keyboard either.
    vscode.window.setStatusBarMessage("Cagara: SQL shown beside the source", 3000);
  }
}

/**
 * The column an SQL preview is already in, if one is open.
 *
 * Read from the tab groups rather than remembered: a column number goes stale
 * as groups are closed, and a preview tab is recognisable by its scheme.
 */
function previewColumn(): vscode.ViewColumn | undefined {
  for (const group of vscode.window.tabGroups.all) {
    for (const tab of group.tabs) {
      const input = tab.input;
      if (input instanceof vscode.TabInputText && input.uri.scheme === SQL_PREVIEW_SCHEME) {
        return group.viewColumn;
      }
    }
  }
  return undefined;
}

/** Run the compiler, capturing both streams. Never rejects; a spawn error is a message. */
async function runCompiler(
  command: string,
  args: string[],
): Promise<{ stdout: string; stderr: string; code: number | null }> {
  return new Promise((resolve) => {
    // A `spawn` that fails emits `error` and then `close`, so a bare resolve in
    // each would settle the promise twice; the first answer is the real one.
    let settled = false;
    const done = (r: { stdout: string; stderr: string; code: number | null }) => {
      if (!settled) {
        settled = true;
        resolve(r);
      }
    };
    let child;
    try {
      child = spawn(command, args, { windowsHide: true });
    } catch (err) {
      done({ stdout: "", stderr: err instanceof Error ? err.message : String(err), code: -1 });
      return;
    }
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (b: Buffer) => (stdout += b.toString()));
    child.stderr?.on("data", (b: Buffer) => (stderr += b.toString()));
    child.on("error", (err: Error) =>
      done({ stdout, stderr: `${stderr}${err.message}`, code: -1 }),
    );
    child.on("close", (code: number | null) => done({ stdout, stderr, code }));
  });
}

/**
 * The first `n` lines of a message, for a notification.
 *
 * A diagnostic includes the offending source line and a caret, which is worth
 * keeping; a whole file's worth of them is not, and an error notification with
 * a hundred lines is unreadable.
 */
function firstLines(text: string, n: number): string {
  const lines = text.split("\n");
  return lines.length <= n ? text : `${lines.slice(0, n).join("\n")}\n…`;
}

/**
 * The formatter's edits, to be applied with the save.
 *
 * The edits are the language server's, asked for through the same command
 * Format Document runs, so `cagara.formatOnSave` and `Shift+Alt+F` produce the
 * same text — including the server's property that only the lines that change
 * are replaced. A server that is not up, or that answers with an error, must
 * not fail the save: the file is still worth writing as it stands.
 */
function formatBeforeSave(document: vscode.TextDocument): Thenable<vscode.TextEdit[]> {
  const editor = vscode.workspace.getConfiguration("editor", document.uri);
  const options = formattingOptions((key) => editor.get(key));
  return Promise.resolve(
    vscode.commands.executeCommand<vscode.TextEdit[]>(
      "vscode.executeFormatDocumentProvider",
      document.uri,
      options,
    ),
  ).then(
    (edits) => edits ?? [],
    () => [],
  );
}

export async function deactivate(): Promise<void> {
  await stop();
}

async function start(context: vscode.ExtensionContext): Promise<void> {
  const server = serverFor(context);
  const run = { command: server.command, args: ["lsp"] };
  const serverOptions: ServerOptions = { run, debug: run };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "cagara" }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.cagara"),
    },
  };
  client = new LanguageClient("cagara", "Cagara Language Server", serverOptions, clientOptions);
  try {
    await client.start();
    announce(context, client, server);
  } catch (err) {
    client = undefined;
    const msg = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(
      `Cagara: could not start \`${server.command} lsp\` (${msg}). Build it with \`cargo build --release\` or set cagara.path.`,
    );
  }
}

/**
 * Say which server is running, in the output channel next to its own log.
 *
 * A bundled server outranks every other candidate, and a development host
 * stages one from the checkout it was launched from: two checkouts, or a stage
 * older than the sources, run a compiler the window gives no sign of. The line
 * is three facts — the path, where it came from, and the file's date — which
 * is enough to see that the server is from another checkout or from last week.
 *
 * The development host also gets a reminder that `Ctrl+Shift+I` is Developer
 * Tools there and Format Document everywhere else: both commands claim the
 * key, and the DevTools binding only exists in a development host, where its
 * weight (250) beats the format binding's (100). Someone formatting from the
 * keyboard in an F5 window therefore opens a debugger instead, which is a
 * minute of DevTools start-up rather than a reformat.
 */
function announce(
  context: vscode.ExtensionContext,
  started: LanguageClient,
  server: ResolvedServer,
): void {
  let fileDate: Date | undefined;
  try {
    fileDate = fs.statSync(server.command).mtime;
  } catch {
    // A bare name for `PATH` to resolve: there is no file to date.
  }
  const channel = started.outputChannel;
  channel.appendLine(`Cagara: ${describeServer(server, fileDate)}`);
  if (context.extensionMode === vscode.ExtensionMode.Development) {
    channel.appendLine(
      "Cagara: development host — Ctrl+Shift+I opens Developer Tools here and " +
        "Shift+Alt+F (or the Command Palette) formats the document.",
    );
  }
}

async function stop(): Promise<void> {
  const c = client;
  client = undefined;
  if (c) {
    await c.stop();
  }
}

/** Gathers what the extension knows and lets `serverpath` decide. */
function serverFor(context: vscode.ExtensionContext): ResolvedServer {
  const exe = process.platform === "win32" ? "cagara.exe" : "cagara";
  return resolveServer({
    configured: vscode.workspace.getConfiguration("cagara").get<string>("path", ""),
    bundled: bundledPath(context, exe),
    // An untrusted workspace never has a repository-local binary executed;
    // a configured path and PATH stay available, so users can still choose a
    // trusted installation.
    trusted: vscode.workspace.isTrusted,
    folders: (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.fsPath),
    exe,
  });
}

/**
 * The server shipped inside a platform-specific VSIX, if this build has one.
 *
 * A release VSIX carries only the target it was packaged for, in
 * `bin/<target>/`; a target-less build has none, so this misses and the
 * caller falls through to a workspace build and then PATH. An architecture no
 * release targets (`ia32`, `armhf`) misses the same way rather than running a
 * binary built for a different CPU.
 */
function bundledPath(context: vscode.ExtensionContext, exe: string): string | undefined {
  const target = vscodeTarget(process.platform, process.arch);
  if (!target) {
    return undefined;
  }
  const candidate = vscode.Uri.joinPath(context.extensionUri, "bin", target, exe).fsPath;
  return fs.existsSync(candidate) ? candidate : undefined;
}
