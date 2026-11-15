//! The `cagara` examples in `docs/` still compile, or are known fragments.
//!
//! `CAGARA.md` and `README.md` are covered by `doc_blocks.rs`, which requires every
//! example in them to compile. The documents under `docs/` are different: several are
//! tutorials, and a tutorial shows *fragments* — a stage on its own, an expression
//! that assumes the table introduced three paragraphs earlier, a deliberately broken
//! line to demonstrate a diagnostic. Requiring those to compile would mean rewriting
//! the prose around the compiler, which is the tail wagging the dog.
//!
//! So the property is a *snapshot*, not a rule: every `cagara` block either compiles
//! or is listed below with the reason. That catches both failures worth catching —
//! an example that used to compile and no longer does, and a new example that was
//! never checked — without pretending a tutorial is a test suite.
//!
//! # Adding an example
//!
//! Write it. If it compiles, nothing to do. If it is a fragment, add it to
//! [`FRAGMENTS`] with the reason. The reason is prose for a reviewer — the test only
//! checks that the *set* has not changed, so a fragment whose prose reason has gone
//! stale is a review problem rather than a build failure.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every ```` ```cagara ```` block in `src`, in document order.
fn cagara_blocks(src: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in src.lines() {
        match current.as_mut() {
            Some(body) if line.starts_with("```") => {
                blocks.push(body.join("\n"));
                current = None;
            }
            Some(body) => body.push(line),
            None if line.trim_end() == "```cagara" => current = Some(Vec::new()),
            None => {}
        }
    }
    blocks
}

/// Every document under `docs/`, sorted, so a failure names a stable file.
fn documents() -> Vec<PathBuf> {
    let dir = repo_root().join("docs");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    files
}

/// Run the compiler over one block, in a scratch file.
fn checks(block: &str, dir: &Path, index: usize) -> bool {
    let file = dir.join(format!("block{index}.cagara"));
    std::fs::write(&file, block).expect("write block");
    let out: Output = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg("check")
        .arg(&file)
        .output()
        .expect("run cagara check");
    out.status.success()
}

/// `path:index` for every block that does not check.
///
/// The identity is the file and the block's position in it, not the block's text:
/// editing an example that is already a fragment should not require a snapshot
/// update, and editing one that compiles must fail this test whether or not the
/// edit kept it compiling.
fn broken_blocks() -> BTreeSet<String> {
    let dir = tempfile::Builder::new()
        .prefix("cagara-docs-")
        .tempdir()
        .expect("create temp dir");
    let mut out = BTreeSet::new();
    for path in documents() {
        let src = std::fs::read_to_string(&path).expect("read a document");
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for (i, block) in cagara_blocks(&src).into_iter().enumerate() {
            if !checks(&block, dir.path(), i) {
                out.insert(format!("{name}:{i}"));
            }
        }
    }
    out
}

/// The blocks that are deliberately not complete programs, with the reason.
///
/// Each entry is a claim about the *document*, and the list is what makes the test
/// meaningful: a fragment is allowed, an unnoticed fragment is not.
const FRAGMENTS: &[(&str, &str)] = &[
    // `DESIGN-REVIEW.md` is a review: it quotes one query, then quotes *wrong*
    // spellings to explain the diagnostics they produce, and quotes a proposal for
    // syntax that does not exist. All of that is the document's point.
    (
        "DESIGN-REVIEW.md:0",
        "uses `orders`/`users`, introduced in prose",
    ),
    (
        "DESIGN-REVIEW.md:1",
        "quotes a pipeline whose semantics the section criticises",
    ),
    (
        "DESIGN-REVIEW.md:2",
        "the SQL the compiler emits for the block above, not a program",
    ),
    (
        "DESIGN-REVIEW.md:3",
        "a deliberately wrong signature, quoted to show the error it produces",
    ),
    ("DESIGN-REVIEW.md:4", "a fragment of the query above"),
    (
        "DESIGN-REVIEW.md:6",
        "proposed syntax (`rename`, `extend`) that the language does not have",
    ),
    // `EXPRESSING-SQL.md` shows a helper, then the pipeline that uses it: the
    // fragment refers to the helper's own parameter.
    (
        "EXPRESSING-SQL.md:0",
        "refers to `a`, the parameter of the helper in the same block, and is a \
         deliberately broken line with its diagnostic in a comment",
    ),
    // `QUERY-ALGEBRA.md` describes the internal relational layer, which is not
    // written in Cagara at all — the block is pseudo-notation for the algebra.
    (
        "QUERY-ALGEBRA.md:0",
        "the relational layer's notation, not a Cagara program",
    ),
];

#[test]
fn every_doc_example_compiles_or_is_a_known_fragment() {
    let broken = broken_blocks();
    let known: BTreeSet<String> = FRAGMENTS.iter().map(|(k, _)| (*k).to_string()).collect();

    let unexpected: Vec<&String> = broken.difference(&known).collect();
    let fixed: Vec<&String> = known.difference(&broken).collect();
    assert!(
        unexpected.is_empty(),
        "these doc examples stopped compiling and are not listed as fragments: \
         {unexpected:?}\nIf the block is a fragment, add it to FRAGMENTS with the \
         reason; if it is a program, fix it."
    );
    assert!(
        fixed.is_empty(),
        "these doc examples compile now, so their FRAGMENTS entries are stale and \
         should go: {fixed:?}"
    );
}

/// A `cagara` fence is spelled exactly, so this test sees every one of them.
///
/// The extractor matches ```` ```cagara ```` at a line start. A block tagged
/// ```` ```Cagara ```` or ```` ```cagara-lang ```` would be silently skipped — an
/// example nobody checks, which is the failure this whole file exists to prevent.
#[test]
fn every_cagara_fence_is_spelled_the_same_way() {
    let mut odd = Vec::new();
    for path in documents() {
        let src = std::fs::read_to_string(&path).expect("read a document");
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for line in src.lines() {
            let t = line.trim_end();
            if !t.starts_with("```") {
                continue;
            }
            let lang = t.trim_start_matches('`');
            // A closing fence is empty; anything else is an opening one.
            if lang.is_empty() {
                continue;
            }
            if lang.to_ascii_lowercase().contains("cagara") && lang != "cagara" {
                odd.push(format!("{name}: ```{lang}"));
            }
        }
    }
    assert!(
        odd.is_empty(),
        "these fences are not tagged `cagara`, so the example test skips them: {odd:?}"
    );
}
