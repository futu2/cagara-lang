// The `cagara.*` settings that are declared `resource` in `package.json`, read
// for one document.
//
// No `vscode` import here, so the scoping can be tested in plain node — the
// same shape as `compile.ts` and `save.ts`. The editor work stays in
// `extension.ts`, which passes `vscode.workspace` and the document's URI.
//
// Scoping is the whole reason this module exists. These settings are
// `resource`-scoped so that each folder of a multi-root workspace can compile
// for its own database; asking for them without a scope returns the window
// value and silently ignores the folder's.

import { splitArgs } from "./compile";

/**
 * The part of `vscode.WorkspaceConfiguration` these lookups use.
 *
 * Only the form that takes a default: every setting read here has one, so the
 * result is never undefined and no caller has to guess at a fallback.
 */
export interface Configuration {
  get<T>(key: string, defaultValue: T): T;
}

/**
 * The part of `vscode.workspace` these lookups use.
 *
 * The scope is generic because the tests have no `vscode.Uri` to hand: they
 * model the folders with strings, while the extension passes the real URI. The
 * type cannot require the argument to be present, so the test that drops it is
 * what keeps the bug from returning.
 */
export interface ConfigurationSource<Scope> {
  getConfiguration(section: string, scope?: Scope): Configuration;
}

/** The compile settings for one document. */
export interface CompileSettings {
  /** The `cagara.dialect` setting. Empty means the compiler's own default. */
  dialect: string;
  /** The `cagara.extraArgs` setting, split on whitespace. */
  extraArgs: string[];
}

/**
 * `cagara.dialect` and `cagara.extraArgs` as they apply to one document.
 *
 * Read with the document's URI: in a multi-root workspace the folder's choice
 * is the one the file belongs to, and an unscoped read compiles folder A's
 * file for the window's dialect while folder A's settings say otherwise.
 */
export function compileSettingsFor<Scope>(
  source: ConfigurationSource<Scope>,
  scope: Scope,
): CompileSettings {
  const config = source.getConfiguration("cagara", scope);
  return {
    dialect: config.get<string>("dialect", ""),
    extraArgs: splitArgs(config.get<string>("extraArgs", "")),
  };
}

/**
 * Whether a save of this document copies its SQL.
 *
 * Scoped like the dialect, because the same window/folder split applies: a
 * folder that opts out must not be copied because the window says yes.
 */
export function copyOnSaveFor<Scope>(
  source: ConfigurationSource<Scope>,
  scope: Scope,
): boolean {
  return source.getConfiguration("cagara", scope).get<boolean>("copyOnSave", false);
}

/**
 * Whether the copied SQL is also shown beside the source.
 *
 * Its own lookup rather than part of `CompileSettings`, because it is read
 * where the SQL is shown, and scoped for the same reason as `copyOnSave`.
 */
export function showSqlOnSaveFor<Scope>(
  source: ConfigurationSource<Scope>,
  scope: Scope,
): boolean {
  return source.getConfiguration("cagara", scope).get<boolean>("showSqlOnSave", false);
}
