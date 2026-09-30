// Starts the language server (`cagara lsp`, over stdio) for `.cagara` files.

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
    vscode.commands.registerCommand("cagara.restartServer", async () => {
      await stop();
      await start();
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("cagara.path")) {
        await stop();
        await start();
      }
    }),
  );
  await start();
}

export async function deactivate(): Promise<void> {
  await stop();
}

async function start(): Promise<void> {
  const command = cagaraPath();
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

/** The configured path, else a trusted workspace build, else PATH. */
function cagaraPath(): string {
  const exe = process.platform === "win32" ? "cagara.exe" : "cagara";
  const configured = vscode.workspace.getConfiguration("cagara").get<string>("path", "").trim();
  if (configured) {
    return configured;
  }
  // Do not execute a repository-local binary in an untrusted workspace.
  // Configured paths and PATH are still available, so users can explicitly
  // choose a trusted installation.
  if (vscode.workspace.isTrusted) {
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      for (const profile of ["release", "debug"]) {
        const candidate = path.join(folder.uri.fsPath, "target", profile, exe);
        if (fs.existsSync(candidate)) {
          return candidate;
        }
      }
    }
  }
  return exe;
}
