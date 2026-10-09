// Starts the language server (`cagara lsp`, over stdio) for `.cagara` files.

import * as fs from "fs";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

import { resolveServer, vscodeTarget, type ResolvedServer } from "./serverpath";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
    vscode.commands.registerCommand("cagara.restartServer", async () => {
      await stop();
      await start(context);
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

export async function deactivate(): Promise<void> {
  await stop();
}

async function start(context: vscode.ExtensionContext): Promise<void> {
  const { command } = serverFor(context);
  const run = { command, args: ["lsp"] };
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
  } catch (err) {
    client = undefined;
    const msg = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(
      `Cagara: could not start \`${command} lsp\` (${msg}). Build it with \`cargo build --release\` or set cagara.path.`,
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
