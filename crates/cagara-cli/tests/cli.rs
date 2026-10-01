//! Behaviour of the `cagara` executable as a user sees it: what it prints on
//! stderr, and the exit code it leaves behind.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch directory that removes itself, so tests do not collide.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cagara-cli-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }

    /// Write `src` as a file and return its path.
    fn write(&self, name: &str, src: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, src).expect("write source");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(path: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg(path)
        .output()
        .expect("run cagara")
}

/// Run the executable with arbitrary arguments.
fn cagara<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_cagara"))
        .args(args)
        .output()
        .expect("run cagara")
}

/// Run with a source file plus extra flags.
fn run_with(path: &PathBuf, extra: &[&str]) -> Output {
    let mut args: Vec<std::ffi::OsString> = vec![path.into()];
    args.extend(extra.iter().map(Into::into));
    cagara(args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A small but complete schema, used by tests that only care about the CLI
/// surface rather than the language.
const SMALL: &str = "t : query { a = int, b = string } = table \"s\" \"t\"\n\
                     q = t & select { x = .a }\n";

/// One failing helper is reached twice — as itself and through the query that
/// uses it — so the same diagnostic used to be printed twice.
#[test]
fn a_shared_failure_is_reported_once() {
    let dir = TempDir::new("dup");
    let f = dir.write(
        "dup.cagara",
        "bad : expr r int -> expr r int = sql \"$0\"\n\
         t : query { a = int } = table \"s\" \"t\"\n\
         q = t & select { x = bad .a }\n",
    );
    let out = run(&f);
    let err = stderr(&out);
    let hits = err.matches("1-based").count();
    assert_eq!(hits, 1, "the same diagnostic printed {hits} times:\n{err}");
    assert!(!out.status.success(), "a failed compile must exit non-zero");
}

/// Deduplicating must not collapse two errors that differ only in position.
#[test]
fn distinct_failures_are_all_reported() {
    let dir = TempDir::new("distinct");
    let f = dir.write(
        "two.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         q1 = t & select { x = .a + \"s\" }\n\
         q2 = t & select { y = .a * \"s\" }\n",
    );
    let out = run(&f);
    let err = stderr(&out);
    assert_eq!(
        err.matches("type mismatch").count(),
        2,
        "both errors must be reported:\n{err}"
    );
    assert!(err.contains(":2:") && err.contains(":3:"), "{err}");
}

/// A failing helper with no user is still reported, once.
#[test]
fn an_unused_failing_definition_is_reported_once() {
    let dir = TempDir::new("unused");
    let f = dir.write(
        "unused.cagara",
        "bad : expr r int -> expr r int = sql \"$0\"\n",
    );
    let out = run(&f);
    let err = stderr(&out);
    assert_eq!(err.matches("1-based").count(), 1, "{err}");
    assert!(!out.status.success(), "{err}");
}

/// A lone `offset` must compile to SQL the target engine accepts: SQLite
/// rejects a bare OFFSET, so the "no limit" has to be spelled out.
#[test]
fn offset_without_limit_is_spelled_per_dialect() {
    let dir = TempDir::new("offset");
    let f = dir.write(
        "off.cagara",
        "t : query { id = int } = table \"public\" \"t\"\n\
         q = t & order [asc .id] & offset 5\n",
    );
    let sql = |dialect: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_cagara"))
            .args([f.as_os_str(), "--only".as_ref(), "q".as_ref()])
            .arg("--dialect")
            .arg(dialect)
            .output()
            .expect("run cagara");
        assert!(
            out.status.success(),
            "{dialect}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let sqlite = sql("sqlite");
    assert!(
        sqlite.contains("LIMIT -1 OFFSET 5"),
        "SQLite needs `LIMIT -1` with a lone OFFSET: {sqlite}"
    );
    let tsql = sql("tsql");
    assert!(
        tsql.contains("OFFSET 5 ROWS FETCH NEXT"),
        "T-SQL needs a FETCH with OFFSET: {tsql}"
    );
}

// ── output modes ──────────────────────────────────────────────────────────

/// `--types` prints one `name : type` line per root definition.
#[test]
fn types_prints_inferred_types() {
    let dir = TempDir::new("types");
    let f = dir.write("t.cagara", SMALL);
    let out = run_with(&f, &["--types"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("q : query { x = int }"), "{text}");
    assert!(text.contains("t : query { a = int, b = string }"), "{text}");
}

/// `--types --only DEF` prints just that definition.
#[test]
fn types_only_selects_one_definition() {
    let dir = TempDir::new("types-only");
    let f = dir.write("t.cagara", SMALL);
    let out = run_with(&f, &["--types", "--only", "q"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("q : query { x = int }"), "{text}");
    assert!(
        !text.contains("t : query"),
        "only `q` was asked for: {text}"
    );
}

/// A `--only` naming nothing is an error, not a silent success.
#[test]
fn only_with_no_match_fails() {
    let dir = TempDir::new("only-missing");
    let f = dir.write("t.cagara", SMALL);
    let out = run_with(&f, &["--types", "--only", "nope"]);
    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no definition named `nope`"),
        "{}",
        stderr(&out)
    );
    // Without `--types`, the message names queries specifically.
    let out = run_with(&f, &["--only", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no query definition named `nope`"),
        "{}",
        stderr(&out)
    );
}

/// `--only` picks one query out of several.
#[test]
fn only_prints_one_query() {
    let dir = TempDir::new("only");
    let f = dir.write(
        "t.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         one = t & select { x = .a }\n\
         two = t & select { y = .a }\n",
    );
    let out = run_with(&f, &["--only", "two"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("-- two"), "{text}");
    assert!(!text.contains("-- one"), "{text}");
}

/// `--pretty` lays the statement out over several lines.
#[test]
fn pretty_formats_the_sql() {
    let dir = TempDir::new("pretty");
    let f = dir.write("t.cagara", SMALL);
    let plain = stdout(&run_with(&f, &["--only", "q"]));
    let pretty = stdout(&run_with(&f, &["--only", "q", "--pretty"]));
    assert!(pretty.lines().count() > plain.lines().count(), "{pretty}");
    assert!(pretty.contains("SELECT"), "{pretty}");
}

/// Each query is printed under a `-- name` header and ends with a semicolon.
#[test]
fn queries_are_labelled_and_terminated() {
    let dir = TempDir::new("labels");
    let f = dir.write("t.cagara", SMALL);
    let text = stdout(&run_with(&f, &["--only", "q"]));
    assert!(text.starts_with("-- q\n"), "{text}");
    assert!(text.trim_end().ends_with(';'), "{text}");
}

/// Several queries are separated by a blank line, so the output stays
/// readable when it is pasted into a client.
#[test]
fn queries_are_separated_by_a_blank_line() {
    let dir = TempDir::new("sep");
    let f = dir.write(
        "t.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         one = t & select { x = .a }\n\
         two = t & select { y = .a }\n",
    );
    let text = stdout(&run(&f));
    assert!(text.contains(";\n\n-- two"), "queries run together: {text}");
}

// ── dialects ──────────────────────────────────────────────────────────────

/// Dialect names are validated up front, with a non-zero exit.
#[test]
fn an_unknown_dialect_is_a_usage_error() {
    let dir = TempDir::new("dialect");
    let f = dir.write("t.cagara", SMALL);
    let out = run_with(&f, &["--dialect", "nope"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("unknown dialect `nope`"), "{err}");
}

/// A dialect switch reaches the emitted SQL.
#[test]
fn the_dialect_changes_the_sql() {
    let dir = TempDir::new("dialect-use");
    let f = dir.write(
        "t.cagara",
        "t : query { a = int, b = string } = table \"s\" \"t\"\n\
         q = t & select { s = .b <> \"x\" }\n",
    );
    let ansi = stdout(&run_with(&f, &["--only", "q"]));
    let mysql = stdout(&run_with(&f, &["--only", "q", "--dialect", "mysql"]));
    let tsql = stdout(&run_with(&f, &["--only", "q", "--dialect", "tsql"]));
    assert!(ansi.contains("||"), "ANSI concatenates with `||`: {ansi}");
    assert!(mysql.contains("CONCAT"), "MySQL needs CONCAT: {mysql}");
    assert!(tsql.contains("CONCAT"), "T-SQL needs CONCAT: {tsql}");
}

// ── usage and errors ──────────────────────────────────────────────────────

/// A missing file, and a bad flag, are usage errors (exit code 2), while a
/// file that exists but does not check is a compile failure (exit code 1).
#[test]
fn usage_errors_exit_2_and_compile_errors_exit_1() {
    let dir = TempDir::new("exit-codes");
    let missing = dir.0.join("does-not-exist.cagara");
    let out = cagara([&missing]);
    assert!(!out.status.success());
    assert!(!stderr(&out).is_empty(), "expected a diagnostic");

    let bad = dir.write("bad.cagara", "q = t & select { x = .a }\n");
    let out = run(&bad);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    let out = cagara([OsStr::new("--nope")]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("usage: cagara"), "{}", stderr(&out));

    let out = cagara::<[&OsStr; 0], &OsStr>([]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// `--help` prints usage on stdout and succeeds.
#[test]
fn help_succeeds_on_stdout() {
    let out = cagara(["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("usage: cagara"), "{}", stdout(&out));
}

/// `--dialect` without a value is a usage error rather than a hang or panic.
#[test]
fn a_flag_missing_its_value_is_rejected() {
    let dir = TempDir::new("missing-value");
    let f = dir.write("t.cagara", SMALL);
    for flag in ["--dialect", "--only"] {
        let out = run_with(&f, &[flag]);
        assert_eq!(out.status.code(), Some(2), "{flag}: {}", stderr(&out));
        assert!(stderr(&out).contains("needs a"), "{flag}: {}", stderr(&out));
    }
}

// ── `cagara fmt` ──────────────────────────────────────────────────────────

/// `fmt` rewrites a file in place and leaves it formatted (idempotent).
#[test]
fn fmt_rewrites_in_place_and_is_idempotent() {
    let dir = TempDir::new("fmt");
    let f = dir.write(
        "t.cagara",
        "q   =   t &    select{x=.a}\nt : query{a=int} = table \"s\" \"t\"\n",
    );
    let out = cagara([OsStr::new("fmt"), f.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let once = std::fs::read_to_string(&f).unwrap();
    let out = cagara([OsStr::new("fmt"), f.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        once,
        std::fs::read_to_string(&f).unwrap(),
        "fmt is not idempotent"
    );
    assert!(!once.contains("select{x=.a}"), "still unformatted: {once}");
}

/// `fmt --check` reports an unformatted file and fails, without rewriting it.
#[test]
fn fmt_check_reports_without_rewriting() {
    let dir = TempDir::new("fmt-check");
    let src = "q   =   t\n";
    let f = dir.write("t.cagara", src);
    let out = cagara([OsStr::new("fmt"), OsStr::new("--check"), f.as_os_str()]);
    assert!(!out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        src,
        "file was rewritten"
    );

    // An already-formatted file passes.
    let f2 = dir.write("ok.cagara", "q = t\n");
    let out = cagara([OsStr::new("fmt"), OsStr::new("--check"), f2.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
}

/// `fmt` on a file with a syntax error reports it and leaves the file alone.
#[test]
fn fmt_leaves_an_unparseable_file_unchanged() {
    let dir = TempDir::new("fmt-bad");
    let src = "q = t & select {\n";
    let f = dir.write("t.cagara", src);
    let out = cagara([OsStr::new("fmt"), f.as_os_str()]);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stderr(&out).contains("error"), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        src,
        "file was rewritten"
    );
}

/// `fmt -` reads stdin and writes the formatted text to stdout.
#[test]
fn fmt_reads_stdin_and_writes_stdout() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .args(["fmt", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cagara fmt -");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(b"q   =   t\n").expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "q = t\n");
}

/// `fmt` with no files is a usage error.
#[test]
fn fmt_without_files_is_a_usage_error() {
    let out = cagara([OsStr::new("fmt")]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("needs files"), "{}", stderr(&out));
}
