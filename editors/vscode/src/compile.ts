// Running `cagara <file>` to get the SQL for a file's entry point.
//
// No `vscode` import here, so the argument building and output handling can be
// tested directly. The interesting parts are both easy to get wrong in a way
// only a real editor would show: which flags go on the command line, and what
// to do with a run that succeeds while printing nothing.

/** The dialect names the compiler accepts, in the order the picker shows. */
export const DIALECTS = [
  "ansi",
  "postgres",
  "mysql",
  "sqlite",
  "duckdb",
  "tsql",
  "bigquery",
  "snowflake",
  "trino",
  "presto",
  "athena",
  "spark",
  "databricks",
  "doris",
  "starrocks",
  "singlestore",
  "redshift",
  "oracle",
  "hive",
  "clickhouse",
  "teradata",
] as const;

export interface CompileRequest {
  /** The file to compile. */
  file: string;
  /** The `cagara.dialect` setting. Empty means the compiler's own default. */
  dialect: string;
  /** The `cagara.extraArgs` setting, split on whitespace. */
  extraArgs?: string[];
}

/**
 * The argument vector for one compile.
 *
 * No `--only`: the compiler prints its entry point by default, and passing
 * `--only main` would ask for that definition on a file that may not have one
 * — turning "a library prints nothing" into "no query definition named
 * `main`", an error for a file that is not wrong.
 */
export function compileArgs(req: CompileRequest): string[] {
  const args: string[] = [];
  const dialect = req.dialect.trim();
  if (dialect) {
    args.push("--dialect", dialect);
  }
  if (req.extraArgs?.length) {
    args.push(...req.extraArgs);
  }
  args.push(req.file);
  return args;
}

/** Split an `extraArgs`-style setting into arguments. */
export function splitArgs(text: string | undefined): string[] {
  return (text ?? "").split(/\s+/).filter((a) => a.length > 0);
}

/** What a finished compile produced, and what the extension should say about it. */
export type CompileOutcome =
  | { kind: "sql"; sql: string }
  | { kind: "empty" }
  | { kind: "failed"; message: string };

/**
 * Classify a finished run.
 *
 * The compiler exits 0 with no output for a file that has no `main` — a
 * library is a legitimate file, not an error. Reporting that as a failure
 * would make saving a helper module pop up an error every time, and reporting
 * it as success would put an empty string on the clipboard. It is its own
 * outcome so the caller can say what actually happened.
 */
export function classify(stdout: string, stderr: string, code: number | null): CompileOutcome {
  if (code !== 0) {
    const message = stderr.trim() || stdout.trim() || `cagara exited with code ${code}`;
    return { kind: "failed", message };
  }
  const sql = stdout.trim();
  if (sql.length === 0) {
    return { kind: "empty" };
  }
  return { kind: "sql", sql };
}

/**
 * The SQL alone, without the `-- name` banner the compiler prints.
 *
 * The banner is useful on a terminal and wrong on a clipboard: it is not SQL,
 * and pasting it into a client that does not accept `--` fails. Only a leading
 * banner is stripped — `--` later in the text is a comment inside the
 * statement, which belongs to it.
 */
export function stripBanner(stdout: string): string {
  const sql = stdout.trim();
  if (!sql.startsWith("--")) {
    return sql;
  }
  const end = sql.indexOf("\n");
  return end === -1 ? "" : sql.slice(end + 1).trim();
}
