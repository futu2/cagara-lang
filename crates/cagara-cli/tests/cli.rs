//! Behaviour of the `cagara` executable as a user sees it: what it prints on
//! stderr, and the exit code it leaves behind.

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

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

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
    let hits = err.matches("placeholders up to $0").count();
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
    assert_eq!(err.matches("placeholders up to $0").count(), 1, "{err}");
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
