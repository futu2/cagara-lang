//! Behaviour of the `cagara` executable as a user sees it: what it prints on
//! stderr, and the exit code it leaves behind.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch directory that removes itself, so tests do not collide.
struct TempDir(tempfile::TempDir);

impl TempDir {
    fn new(tag: &str) -> Self {
        TempDir(
            tempfile::Builder::new()
                .prefix(&format!("cagara-cli-test-{tag}-"))
                .tempdir()
                .expect("create temp dir"),
        )
    }

    /// Write `src` as a file and return its path.
    fn write(&self, name: &str, src: &str) -> PathBuf {
        let p = self.0.path().join(name);
        std::fs::write(&p, src).expect("write source");
        p
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

/// A scalar `main` used to compile cleanly and print nothing. It is now an
/// error: a file's entry point is a query. `--only` can still inspect a scalar
/// helper, and a library is still allowed to have no `main` at all.
#[test]
fn main_must_be_a_query() {
    let dir = TempDir::new("main-scalar");
    let f = dir.write("scalar.cagara", "main = 1\n");
    let out = run(&f);
    let err = stderr(&out);
    assert!(!out.status.success(), "scalar main must fail:\n{err}");
    assert!(
        err.contains("`main` must be a query") && err.contains("int"),
        "{err}"
    );
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));

    // A library without `main` remains a normal outcome.
    let lib = dir.write("lib.cagara", "q = 1\n");
    let out = run(&lib);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

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

/// A file is compiled for its `main`, not for every definition in it.
///
/// Helpers are not answers: printing them buried the one statement the file
/// was written for, and made reading a module of table declarations dump its
/// whole schema.
#[test]
fn only_main_is_printed() {
    let dir = TempDir::new("mainonly");
    let f = dir.write(
        "t.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         helper = t & select { x = .a }\n\
         main = t & select { y = .a }\n",
    );
    let text = stdout(&run(&f));
    assert!(text.contains("-- main"), "{text}");
    assert!(text.contains("AS y"), "{text}");
    assert!(
        !text.contains("-- helper") && !text.contains("AS x"),
        "a helper was printed as an answer: {text}"
    );
}

/// A file with no `main` is a library, so it compiles and prints nothing —
/// and, being a successful compile, says so with exit code 0.
#[test]
fn a_file_without_main_prints_nothing_and_succeeds() {
    let dir = TempDir::new("nomain");
    let f = dir.write(
        "lib.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         helper = t & select { x = .a }\n",
    );
    let out = run(&f);
    assert_eq!(stdout(&out), "", "a library printed SQL");
    assert_eq!(stderr(&out), "", "silence is not a diagnostic");
    assert!(out.status.success(), "a library is not a failure");
}

/// Printing nothing must not mean checking nothing: a broken helper in a
/// library is still a broken file, and a silently-green build would hide it.
#[test]
fn a_broken_helper_is_reported_even_when_nothing_is_printed() {
    let dir = TempDir::new("brokenlib");
    let f = dir.write(
        "lib.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         broken = t & select { .nonexistent }\n",
    );
    let out = run(&f);
    let err = stderr(&out);
    assert_eq!(stdout(&out), "", "nothing to print, so nothing printed");
    assert!(
        err.contains("nonexistent"),
        "the error was swallowed: {err}"
    );
    assert!(!out.status.success(), "a broken library must exit non-zero");
}

/// `--only` still reads one helper, which is how a definition that is not the
/// entry point is inspected.
#[test]
fn only_reads_a_definition_that_is_not_main() {
    let dir = TempDir::new("onlyhelper");
    let f = dir.write(
        "t.cagara",
        "t : query { a = int } = table \"s\" \"t\"\n\
         helper = t & select { x = .a }\n\
         main = t & select { y = .a }\n",
    );
    let text = stdout(&run_with(&f, &["--only", "helper"]));
    assert!(
        text.contains("-- helper") && text.contains("AS x"),
        "{text}"
    );
    assert!(!text.contains("AS y"), "{text}");
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
         q = t & select { s = .b ++ \"x\" }\n",
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
    let missing = dir.0.path().join("does-not-exist.cagara");
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

/// `--version` and `-V` print the version and nothing else, from any
/// subcommand position, since it is the only information flag there is.
#[test]
fn version_is_printed_for_both_spellings() {
    let expected = format!("cagara {}\n", env!("CARGO_PKG_VERSION"));
    for args in [vec!["--version"], vec!["-V"], vec!["fmt", "--version"]] {
        let out = cagara(args.iter().copied());
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(stdout(&out), expected, "for {args:?}");
        assert!(stderr(&out).is_empty(), "{}", stderr(&out));
    }
}

/// `cagara -` compiles stdin, so a query can come from a pipe.
#[test]
fn compile_reads_stdin() {
    let src = "t : query { a = int } = table \"s\" \"t\"\nmain = t & select { x = .a }\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cagara -");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(src.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("SELECT"), "{}", stdout(&out));
}

/// An error in stdin text names `<stdin>`, not a path that does not exist.
#[test]
fn stdin_errors_are_reported_against_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cagara -");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"main = nope\n")
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(!out.status.success(), "a failed compile must exit non-zero");
    let err = stderr(&out);
    assert!(err.contains("<stdin>:"), "{err}");
    assert!(!err.contains("No such file"), "{err}");
}

/// A missing file is one line, without a `1:1` pointing at a line that does
/// not exist.
#[test]
fn a_missing_file_has_no_position() {
    let dir = TempDir::new("missing");
    let missing = dir.0.path().join("absent.cagara");
    let out = run(&missing);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("cannot read file"), "{err}");
    assert!(!err.contains(":1:1"), "a missing file has no line: {err}");
}

/// Diagnostics name the file the way the user typed it, not the canonical
/// absolute path the workspace loaded it as.
#[test]
fn diagnostics_use_the_path_as_typed() {
    let dir = TempDir::new("rebase");
    let f = dir.write(
        "rel.cagara",
        "t : query { a = int } = table \"s\" \"t\"\nmain = t & where (.nope > 1)\n",
    );
    // Run from the temp directory and name the file relatively.
    let out = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg("rel.cagara")
        .current_dir(dir.0.path())
        .output()
        .expect("run cagara");
    let err = stderr(&out);
    assert!(err.contains("rel.cagara:"), "{err}");
    let abs = f.canonicalize().expect("canonical");
    assert!(
        !err.contains(&abs.display().to_string()),
        "the absolute path leaked into a diagnostic: {err}"
    );
}

/// A UTF-8 BOM is transport metadata: compile it, and preserve it when
/// formatting so a Windows file does not silently change encoding.
#[test]
fn bom_input_compiles_and_is_preserved_by_fmt() {
    let dir = TempDir::new("bom");
    let f = dir.write(
        "bom.cagara",
        "\u{feff}t : query { a = int } = table \"s\" \"t\"\nmain=t\n",
    );
    let out = run(&f);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("SELECT a FROM s.t;"),
        "{}",
        stdout(&out)
    );

    let out = cagara([OsStr::new("fmt"), f.as_os_str()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = std::fs::read_to_string(&f).expect("read formatted file");
    assert!(
        text.starts_with('\u{feff}'),
        "fmt dropped the BOM: {text:?}"
    );
    assert!(text.contains("main = t"), "fmt did not format: {text:?}");
}

/// `cagara lsp --help` explains the subcommand instead of rejecting the flag.
#[test]
fn lsp_help_succeeds() {
    for flag in ["--help", "-h"] {
        let out = cagara(["lsp", flag]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(stdout(&out).contains("cagara lsp"), "{}", stdout(&out));
        assert!(stderr(&out).is_empty(), "{}", stderr(&out));
    }
}

/// `--list` prints every root query, and `--arg` applies a definition to
/// literals — both are ways to read a file of many definitions.
///
/// `--arg` needs no compiler support: it builds one more definition,
/// `__cagara_args = <selected> <literal> …`, and compiles that, so a wrong
/// argument type is reported by the ordinary checker and the output is labelled
/// with the definition the user asked for, not the wrapper.
#[test]
fn list_prints_every_query_and_arg_applies_literals() {
    let dir = TempDir::new("list-arg");
    let path = dir.write(
        "q.cagara",
        "t : query { id = int, d = date } = table \"s\" \"t\"\n\
         a = t & where (.id > 1)\n\
         b = t & select { .id }\n",
    );
    let out = run_with(&path, &["--list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("-- a\n"), "{text}");
    assert!(text.contains("-- b\n"), "{text}");
    assert!(text.contains("WHERE (id > 1)"), "{text}");

    // A definition applied to a literal: a string literal widens to `date`.
    let path = dir.write(
        "since.cagara",
        "t : query { id = int, d = date } = table \"s\" \"t\"\n\
         since = (day : date) => t & where (.d >= day)\n",
    );
    let out = run_with(&path, &["--only", "since", "--arg", "2024-01-01"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("-- since\n"), "{text}");
    assert!(text.contains("(d >= '2024-01-01')"), "{text}");

    // A number stays a number, and `main` may itself be the function.
    let path = dir.write(
        "n.cagara",
        "t : query { id = int } = table \"s\" \"t\"\n\
         main = (n : int) => t & where (.id > n)\n",
    );
    let out = run_with(&path, &["--arg", "5"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("(id > 5)"), "{}", stdout(&out));

    // A value the definition cannot take fails like any other call.
    let out = run_with(&path, &["--arg", "2024-01-01"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(!stderr(&out).is_empty(), "expected a diagnostic");
}

/// `--only alias.name` reads a definition out of an imported module, and asks
/// for `--arg` when that definition is a function.
///
/// Both go through the same one-line wrapper (`__cagara_args = <name> …`), so
/// an unknown module member is the ordinary "module `s` has no definition"
/// error rather than a CLI-specific one.
#[test]
fn only_reaches_into_an_imported_module() {
    let dir = TempDir::new("only-import");
    std::fs::create_dir_all(dir.0.path().join("lib")).expect("lib dir");
    dir.write(
        "lib/s.cagara",
        "users : query { id = int, name = string } = table \"p\" \"u\"\n\
         byId = (i : int) => users & where (.id == i)\n",
    );
    let path = dir.write(
        "top.cagara",
        "import \"lib/s.cagara\" as s\nmain = s.users\n",
    );

    let out = run_with(&path, &["--only", "s.byId", "--arg", "3"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("-- s.byId\n"), "{text}");
    assert!(text.contains("WHERE (id = 3)"), "{text}");

    // A function with no arguments supplied says what to do.
    let out = run_with(&path, &["--only", "s.byId"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("is a function of 1 argument(s)") && stderr(&out).contains("--arg"),
        "{}",
        stderr(&out)
    );

    // An unknown member is the ordinary module error.
    let out = run_with(&path, &["--only", "s.nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("has no definition `nope`"),
        "{}",
        stderr(&out)
    );

    // `--types` labels the result with the name that was asked for.
    let out = run_with(&path, &["--only", "s.byId", "--types"]);
    let text = stdout(&out);
    assert!(text.starts_with("s.byId : "), "{text}");
}

/// `cagara check` answers "is this tree sound", one file per run, and says so
/// in a machine-readable form with `--json`.
///
/// It checks each file as a *library*, so a directory of table declarations is
/// reportable without one of them being `main`. Exit codes distinguish "found a
/// diagnostic" (1) from "you called me wrong" (2), which is what a hook needs.
#[test]
fn check_reports_a_whole_tree() {
    let dir = TempDir::new("check-tree");
    dir.write(
        "good.cagara",
        "t : query { id = int } = table \"s\" \"t\"\nq = t & select { .id }\n",
    );
    std::fs::create_dir_all(dir.0.path().join("sub")).expect("sub dir");
    dir.write(
        "sub/bad.cagara",
        "t : query { id = int } = table \"s\" \"t\"\nq = t & select { .nope }\n",
    );
    // A dot directory is an artifact or a cache, not part of the tree.
    std::fs::create_dir_all(dir.0.path().join(".cache")).expect("cache dir");
    dir.write(".cache/ignored.cagara", "q = .nope\n");

    let out = cagara([std::ffi::OsString::from("check"), dir.0.path().into()]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let err = stderr(&out);
    assert!(err.contains("bad.cagara:2:9: error:"), "{err}");
    assert!(err.contains("no column `nope`"), "{err}");
    assert!(
        !err.contains("ignored.cagara"),
        "dot dirs are skipped: {err}"
    );

    // JSON: one object per line, with the words a consumer needs.
    let out = cagara([
        std::ffi::OsString::from("check"),
        std::ffi::OsString::from("--json"),
        dir.0.path().into(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    let line = lines[0];
    assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
    assert!(line.contains("\"severity\":\"error\""), "{line}");
    assert!(
        line.contains("\"line\":2") && line.contains("\"column\":9"),
        "{line}"
    );
    assert!(line.contains("no column `nope`"), "{line}");

    // A clean file exits 0 and says nothing.
    let out = cagara([
        std::ffi::OsString::from("check"),
        dir.0.path().join("good.cagara").into_os_string(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).is_empty() && stderr(&out).is_empty());
}

/// `--param name=value` supplies a definition the file leaves to its caller.
///
/// A parameter is an ordinary definition written at the end of the file, so the
/// value is part of the compilation: it is checked against every use, and a
/// wrong type is a type error at the *flag* rather than an SQL surprise. The
/// literal is written the way `--arg` writes one — a number, `true`/`false`, or
/// a quoted string, which widens to `date` where the signature says so.
#[test]
fn param_supplies_a_definition_the_file_leaves_open() {
    let dir = TempDir::new("param");
    let path = dir.write(
        "q.cagara",
        "orders : query { id = int, amount = float, created_at = date } = table \"s\" \"o\"\n\
         main = orders & where (.created_at >= since) & where (.amount > floor_amount)\n",
    );

    // Without the flags the names do not exist: running the file alone says so.
    let out = run(&path);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("unknown name `since`"),
        "{}",
        stderr(&out)
    );

    // A string literal widens to `date`; a number is a number.
    let out = run_with(
        &path,
        &[
            "--param",
            "since=2024-01-01",
            "--param",
            "floor_amount=10.5",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("(created_at >= '2024-01-01')"), "{text}");
    assert!(text.contains("(amount > 10.5)"), "{text}");

    // A signature can be given when the literal alone is too vague.
    let out = run_with(
        &path,
        &[
            "--param",
            "since:date=2024-01-01",
            "--param",
            "floor_amount:float=10",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("(amount > 10)"), "{}", stdout(&out));

    // A value that does not fit is reported against the flag, not against a
    // line of a file the user did not write.
    let out = run_with(
        &path,
        &[
            "--param",
            "since:int=2024-01-01",
            "--param",
            "floor_amount=1",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("`--param since:int=2024-01-01`"), "{err}");
    assert!(err.contains("expected int, found string"), "{err}");

    // A malformed flag is a usage error, exit 2.
    let out = run_with(&path, &["--param", "since"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--param name=value"),
        "{}",
        stderr(&out)
    );
    let out = run_with(&path, &["--param", "1x=2"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// The editor and the command line report the same thing, in the same place.
///
/// Both read `compile_diagnostics`, but the two paths differ after it: the CLI
/// prints 1-based line and column and the server converts byte offsets to
/// UTF-16 positions. A test where the two agree is the only thing that keeps a
/// column off-by-one or a dropped diagnostic from reaching users twice — once
/// in CI and once under the cursor, saying different things.
#[test]
fn the_editor_and_the_command_line_agree() {
    let dir = TempDir::new("parity");
    // A non-BMP character before the errors makes a byte column and a UTF-16
    // column differ, which is exactly the case the conversion can get wrong.
    let src = "t : query { id = int, name = string } = table \"s\" \"t\"\n\
               # 🎉🎉\n\
               a = t & select { .nope }\n\
               b = t & select { x = upper .id }\n\
               c = t & select { .id } & where (.id)\n";
    let path = dir.write("p.cagara", src);

    // What the command line says.
    let out = cagara([
        std::ffi::OsString::from("check"),
        std::ffi::OsString::from("--json"),
        path.clone().into_os_string(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let mut from_cli: Vec<(u32, u32, String, String)> = stdout(&out)
        .lines()
        .map(|line| {
            // The objects are written by hand, so a field lookup is enough.
            let get = |k: &str| -> String {
                let at = line
                    .find(&format!("\"{k}\":"))
                    .unwrap_or_else(|| panic!("{k}: {line}"));
                let rest = &line[at + k.len() + 3..];
                let end = rest.find([',', '}']).unwrap_or(rest.len());
                rest[..end].trim_matches('"').to_string()
            };
            // The message is last, so it is read from its own delimiter to the
            // end rather than up to the next comma — these messages contain
            // commas, and a diff-style one contains newlines too.
            let start = line.find("\"message\":\"").expect("message") + "\"message\":\"".len();
            let end = line.rfind("\"}").expect("end");
            let message = line[start..end].replace("\\n", "\n").replace("\\\"", "\"");
            (
                get("line").parse().expect("line"),
                get("column").parse().expect("column"),
                get("severity"),
                message,
            )
        })
        .collect();
    from_cli.sort();

    // What the editor is told.
    let ws = cagara_hir::workspace::Workspace::open(&path);
    let mut from_lsp: Vec<(u32, u32, String, String)> = cagara_lsp::analysis::diagnostics(&ws)
        .into_iter()
        .map(|(range, message, severity)| {
            // 0-based line and UTF-16 column, which is what the protocol counts.
            (
                range.start.line + 1,
                range.start.character + 1,
                severity.as_str().to_string(),
                message,
            )
        })
        .collect();
    from_lsp.sort();

    assert_eq!(
        from_lsp, from_cli,
        "the two must report the same diagnostics"
    );
    assert_eq!(from_cli.len(), 3, "{from_cli:?}");
    // The emoji line is why the columns are converted at all: a byte column
    // would be one higher than a UTF-16 column after `# 🎉🎉`.
    assert!(
        from_cli
            .iter()
            .all(|(line, _, _, _)| *line == 3 || *line == 4 || *line == 5),
        "{from_cli:?}"
    );
}

/// A warning is reported, and the run still succeeds.
///
/// Some diagnostics describe what the program *means* rather than a mistake: a
/// join whose inputs share a column keeps the left one, which is exactly what
/// its output row says, and the reader is told that the right one cannot be
/// read afterwards. Failing the build over that forced a `select` on every join
/// between two tables that both have an `id`, so it is a warning — printed with
/// `warning:`, present in `--json` with `"severity":"warning"`, and not an exit
/// code.
#[test]
fn a_warning_is_reported_and_does_not_fail_the_build() {
    let dir = TempDir::new("warn");
    let path = dir.write(
        "q.cagara",
        "users : query { id = int, name = string } = table \"s\" \"u\"\n\
         orders : query { id = int, user_id = int } = table \"s\" \"o\"\n\
         q = users & innerJoin orders (.<id == .>user_id)\n",
    );

    // The query compiles: `--list` prints its SQL and exits 0.
    let out = cagara([path.clone().into_os_string()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("warning: this join has `id` on both sides"),
        "{err}"
    );
    assert!(!err.contains("error:"), "{err}");
    // `q` is not `main`, so the SQL needs `--only`; it compiles all the same.
    let out = cagara([
        path.clone().into_os_string(),
        std::ffi::OsString::from("--only"),
        std::ffi::OsString::from("q"),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("INNER JOIN"), "{}", stdout(&out));

    // And `check`, which is what a hook runs, agrees: warning out, exit 0.
    let out = cagara([
        std::ffi::OsString::from("check"),
        std::ffi::OsString::from("--json"),
        path.clone().into_os_string(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let line = stdout(&out);
    assert!(line.contains("\"severity\":\"warning\""), "{line}");
    assert!(line.contains("on both sides"), "{line}");

    // `check` without `--json` says `warning:` too, not `error:`.
    let out = cagara([std::ffi::OsString::from("check"), path.into_os_string()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains(": warning: "), "{err}");
    assert!(!err.contains(": error: "), "{err}");
}
