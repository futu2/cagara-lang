//! `cagara doc`: the reference a file carries in its `##` and `#!` comments.
//!
//! The behaviour under test is the whole path — marker, attachment to the
//! definition, inferred signature, and the two renderings — because the point
//! of the feature is that the words a writer puts next to a definition are the
//! words a reader gets somewhere else.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch directory that removes itself, so tests do not collide.
struct TempDir(tempfile::TempDir);

impl TempDir {
    fn new(tag: &str) -> Self {
        TempDir(
            tempfile::Builder::new()
                .prefix(&format!("cagara-doc-{tag}-"))
                .tempdir()
                .expect("create temp dir"),
        )
    }

    fn write(&self, name: &str, src: &str) -> PathBuf {
        let p = self.0.path().join(name);
        std::fs::write(&p, src).expect("write source");
        p
    }
}

fn cagara(args: Vec<std::ffi::OsString>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cagara"))
        .args(args)
        .output()
        .expect("run cagara")
}

/// `cagara doc <file> [flags...]`.
fn doc(file: &Path, flags: &[&str]) -> Output {
    let mut args: Vec<std::ffi::OsString> = vec!["doc".into(), file.into()];
    args.extend(flags.iter().map(std::ffi::OsString::from));
    cagara(args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// A documented query file: one definition with a doc block, one without.
const DOCUMENTED: &str = "#! The counting file.\n\
                          \n\
                          ## Counts the rows.\n\
                          rows : query { a = int } = table \"s\" \"t\"\n\
                          \n\
                          ## The answer.\n\
                          main = rows & select { x = .a }\n";

#[test]
fn the_reference_prints_the_file_doc_signatures_and_bodies() {
    let dir = TempDir::new("markdown");
    let f = dir.write("q.cagara", DOCUMENTED);
    let out = doc(&f, &[]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.starts_with("# "), "a file heading comes first: {text}");
    assert!(text.contains("The counting file."), "{text}");
    assert!(text.contains("## `rows`"), "{text}");
    assert!(text.contains("rows : query { a = int }"), "{text}");
    assert!(text.contains("Counts the rows."), "{text}");
    assert!(text.contains("## `main`"), "{text}");
    assert!(text.contains("main : query { x = int }"), "{text}");
}

/// JSON says the same thing in a shape a tool can read: one object per module,
/// one entry per definition, the doc text as a string or `null`.
#[test]
fn json_carries_the_same_words() {
    let dir = TempDir::new("json");
    let f = dir.write("q.cagara", DOCUMENTED);
    let out = doc(&f, &["--json"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains(r#""path""#), "{text}");
    assert!(text.contains(r#""doc": "The counting file.""#), "{text}");
    assert!(text.contains(r#""name": "rows""#), "{text}");
    assert!(text.contains(r#""type": "query { a = int }""#), "{text}");
    assert!(text.contains(r#""doc": "Counts the rows.""#), "{text}");
}

/// `--check` reports what is undocumented and **succeeds**: a missing doc
/// comment is a writing task, not a build failure.
#[test]
fn check_warns_and_still_succeeds() {
    let dir = TempDir::new("check");
    let f = dir.write(
        "q.cagara",
        "rows : query { a = int } = table \"s\" \"t\"\n\
         ## Documented.\n\
         main = rows & select { x = .a }\n",
    );
    let out = doc(&f, &["--check"]);
    let err = stderr(&out);
    assert!(out.status.success(), "a warning must not fail: {err}");
    assert_eq!(
        err.matches("has no doc comment").count(),
        1,
        "exactly the undocumented definition is reported:\n{err}"
    );
    assert!(err.contains("`rows`"), "{err}");
    assert!(err.contains(":1:"), "the warning names the line:\n{err}");
    assert!(!err.contains("`main`"), "`main` is documented:\n{err}");
    // Warnings go to stderr, so a redirected reference is unaffected.
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

/// `--check` on a fully documented file says nothing at all.
#[test]
fn check_is_silent_when_everything_is_documented() {
    let dir = TempDir::new("check-clean");
    let f = dir.write("q.cagara", DOCUMENTED);
    let out = doc(&f, &["--check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
}

/// An overload set is one thing to document: the report names it once, and the
/// reference prints one section with both signatures.
#[test]
fn an_overload_set_is_documented_and_reported_once() {
    let dir = TempDir::new("overload");
    let f = dir.write(
        "q.cagara",
        "## Adds a column up.\n\
         total : expr r int -> agg (expr r (maybe int)) = sql \"SUM($1)\"\n\
         total : expr r float -> agg (expr r (maybe float)) = sql \"SUM($1)\"\n\
         t : query { a = int } = table \"s\" \"t\"\n\
         main = t & agg { s = total .a }\n",
    );
    let out = doc(&f, &["--check"]);
    let err = stderr(&out);
    assert_eq!(
        err.matches("has no doc comment").count(),
        2,
        "only `t` and `main` are undocumented:\n{err}"
    );
    assert!(!err.contains("`total`"), "{err}");

    let out = doc(&f, &[]);
    let text = stdout(&out);
    assert_eq!(text.matches("## `total`").count(), 1, "{text}");
    assert_eq!(text.matches("Adds a column up.").count(), 1, "{text}");
    assert!(text.contains("total : expr r int -> agg (expr r (maybe int))"));
    assert!(text.contains("total : expr r float -> agg (expr r (maybe float))"));
}

/// The formatter keeps the attachment: `##` lines stay above their definition,
/// and the reference reads the same after `cagara fmt` as before it.
#[test]
fn documents_survive_the_formatter() {
    let dir = TempDir::new("fmt");
    let f = dir.write(
        "q.cagara",
        "##   Deliberately   spaced.\nrows : query { a = int } = table \"s\" \"t\"\n\n## Second.\nmain = rows & select { x = .a }\n",
    );
    let before = doc(&f, &[]);
    assert!(before.status.success(), "{}", stderr(&before));

    let formatted = cagara(vec!["fmt".into(), f.clone().into()]);
    assert!(formatted.status.success(), "{}", stderr(&formatted));

    let after = doc(&f, &[]);
    assert!(after.status.success(), "{}", stderr(&after));
    assert_eq!(
        stdout(&before),
        stdout(&after),
        "formatting must not change the reference"
    );
    // The doc text itself is not reformatted: it is prose, and the writer's
    // spacing inside it is theirs.
    assert!(
        stdout(&after).contains("  Deliberately   spaced."),
        "{}",
        stdout(&after)
    );
}

/// A file that does not parse is reported and prints no reference: a list of
/// definitions from a broken parse is not something to publish.
#[test]
fn a_broken_file_is_reported_and_prints_nothing() {
    let dir = TempDir::new("broken");
    let f = dir.write("q.cagara", "## Docs.\nmain = = 1\n");
    let out = doc(&f, &[]);
    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).is_empty(), "the syntax error is reported");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

/// `--prelude` documents the built-in prelude, which is the same file the
/// compiler loads — so the reference cannot describe something else.
#[test]
fn the_prelude_is_available_without_a_file() {
    let out = cagara(vec!["doc".into(), "--prelude".into()]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("# <prelude>"), "{text}");
    assert!(text.contains("## `where`"), "{text}");
    assert!(text.contains("Filter rows"), "{text}");
}

/// Every definition in the prelude is documented.
///
/// This is the gate on *our* file, not on a user's: `cagara doc --check` warns
/// instead of failing, but the prelude ships as the language's own reference,
/// so a new definition added without a `##` block should be caught here.
#[test]
fn the_prelude_documents_every_definition() {
    let out = cagara(vec!["doc".into(), "--prelude".into(), "--check".into()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).is_empty(),
        "the prelude has undocumented definitions:\n{}",
        stderr(&out)
    );
}

/// The same gate for the examples, which are copied from and read as models.
#[test]
fn the_examples_document_every_definition() {
    let root = repo_root().join("examples");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("examples directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "cagara"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no examples found");
    let mut args: Vec<std::ffi::OsString> = vec!["doc".into(), "--check".into()];
    args.extend(files.iter().map(|p| p.clone().into()));
    let out = cagara(args);
    // `errors.cagara` fails to compile by design, and its type errors are
    // reported here; the documentation report is what this test reads, so the
    // assertion is about undocumented definitions rather than the exit code.
    assert!(
        !stderr(&out).contains("has no doc comment"),
        "{}",
        stderr(&out)
    );
}

/// `-` is stdin, so a file can be documented in a pipeline.
#[test]
fn stdin_is_accepted() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .args(["doc", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cagara");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(DOCUMENTED.as_bytes()).expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("The counting file."), "{text}");
}
