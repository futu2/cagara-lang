// Starts the language server (`cagara lsp`, over stdio) for `.cagara` files,
// and can compile the active file to SQL and put it on the clipboard.

import { spawn } from "child_process";
import * as fs from "fs";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

import { classify, compileArgs, splitArgs, stripBanner } from "./compile";
import { formattingOptions, shouldFormat } from "./save";
import { describeServer, resolveServer, vscodeTarget, type ResolvedServer } from "./serverpath";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
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
 * the SQL can be pasted elsewhere, or open it in an untitled buffer to be read.
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
      // The manual command opens the SQL, because running it is a request to
      // see the result. On save that would be a window per save, so it is
      // opt-in, and the clipboard stays the default so the SQL can be pasted
      // without anything moving.
      const show = trigger === "cagara.copySql" || config.get<boolean>("showSqlOnSave", false);
      if (show) {
        const shown = await vscode.workspace.openTextDocument({
          content: sql,
          language: "sql",
        });
        await vscode.window.showTextDocument(shown, {
          preview: true,
          viewColumn: vscode.ViewColumn.Beside,
        });
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
