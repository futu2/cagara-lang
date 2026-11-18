//! The `cagara` examples in `docs/` still compile, or are known fragments — and the
//! documents a reader can reach are the ones that track the code.
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

/// Every document under `docs/`, including `docs/research/`, sorted by path.
///
/// Recursive on purpose. `docs/research/` holds the dated surveys the redesign
/// rested on — historical records rather than documentation — and their examples are
/// still examples: a block in one of them that stops compiling is the same defect as a
/// block in `LEARN.md` that does. Moving them out of the top level is about what a
/// reader sees first, not about what is checked.
fn documents() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "md") {
                out.push(path);
            }
        }
    }
    let dir = repo_root().join("docs");
    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();
    files
}

/// A document's name as `FRAGMENTS` and the failure messages spell it: its path
/// relative to `docs/`, with `/` separators.
fn doc_name(path: &Path) -> String {
    let root = repo_root().join("docs");
    path.strip_prefix(&root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
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
        let name = doc_name(&path);
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
        "research/DESIGN-REVIEW.md:0",
        "uses `orders`/`users`, introduced in prose",
    ),
    (
        "research/DESIGN-REVIEW.md:1",
        "quotes a pipeline whose semantics the section criticises",
    ),
    (
        "research/DESIGN-REVIEW.md:2",
        "the SQL the compiler emits for the block above, not a program",
    ),
    (
        "research/DESIGN-REVIEW.md:3",
        "a deliberately wrong signature, quoted to show the error it produces",
    ),
    (
        "research/DESIGN-REVIEW.md:4",
        "a fragment of the query above",
    ),
    (
        "research/DESIGN-REVIEW.md:5",
        "quotes the prelude's `lpad` signature as it was when the review was written \
         (bare type variables, before the `?` sigil); it is a dated record, not a program",
    ),
    (
        "research/DESIGN-REVIEW.md:6",
        "proposed syntax (`rename`, `extend`) that the language does not have",
    ),
    // `EXPRESSING-SQL.md` shows a helper, then the pipeline that uses it: the
    // fragment refers to the helper's own parameter.
    (
        "research/EXPRESSING-SQL.md:0",
        "refers to `a`, the parameter of the helper in the same block, and is a \
         deliberately broken line with its diagnostic in a comment",
    ),
    // `QUERY-ALGEBRA.md` describes the internal relational layer, which is not
    // written in Cagara at all — the block is pseudo-notation for the algebra.
    (
        "research/QUERY-ALGEBRA.md:0",
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

/// Every name the prelude defines is in the manual that claims to list them.
///
/// `CAGARA.md` §7 is introduced as "the exact names and argument order" and tells
/// an agent to "use only names from §7" — so a name missing from it is a name a
/// reader is told does not exist. That happened at scale: `rollup`, `cube`,
/// `crossJoin`, `markExists`, `listAt`, the log family and the whole `*Between`
/// family were all defined and all absent, and `cagara doc --prelude` printed
/// them the whole time. This is the drift made into a build failure.
///
/// The check is one-way on purpose. The manual may name things that are not
/// prelude functions — stage names, types, `main` — but it may not omit one.
#[test]
fn every_prelude_name_is_in_the_reference() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("CAGARA.md")).expect("CAGARA.md");
    let out = Command::new(env!("CARGO_BIN_EXE_cagara"))
        .args(["doc", "--prelude"])
        .output()
        .expect("run cagara doc --prelude");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    // The catalog prints one `## `name`` heading per entry.
    let mut missing: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("## `") else {
            continue;
        };
        let Some(name) = rest.split('`').next() else {
            continue;
        };
        // An operator is written `_+_` in the prelude and `+` in prose, so the
        // manual is searched for the spelling a reader would recognise.
        let spelled = name.trim_start_matches('_').trim_end_matches('_');
        // A table cell escapes the pipe (`&\|`), and an operator's spelling in
        // prose has no underscores, so both spellings are tried.
        let escaped = spelled.replace('|', "\\|");
        let found = doc.contains(name)
            || (!spelled.is_empty()
                && (doc.contains(&format!("`{spelled}`"))
                    || doc.contains(&format!("`{escaped}`"))
                    || doc.contains(&escaped)));
        if !found {
            missing.push(name.to_string());
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "these prelude names are absent from CAGARA.md §7, which tells a reader \
         only those names exist: {missing:?}"
    );
}

/// The catalog spells a type variable the way the language does.
///
/// `CAGARA.md` §7 shows each name with its signature, and a reader treats those as
/// something to copy. They were written `_&_ : a -> (?a -> ?b) -> ?b` — bare
/// letters beside `?` sigils, inherited from before the `?a` change — and a bare
/// letter is a type *constructor* name, so `id : a -> a` is an error and those 86
/// lines taught a notation no file accepts. The catalog has two documented
/// shorthands (`same`, `a|b`), but the *sigil* is not one of them.
#[test]
fn the_catalog_writes_type_variables_the_way_the_language_does() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("CAGARA.md")).expect("CAGARA.md");
    let section: Vec<&str> = doc
        .lines()
        .skip_while(|l| !l.starts_with("### 7.1"))
        .take_while(|l| !l.starts_with("## 8."))
        .collect();
    // Real type names, which are bare by definition.
    let real = [
        "int",
        "float",
        "string",
        "bool",
        "date",
        "timestamp",
        "decimal",
        "list",
        "maybe",
        "expr",
        "agg",
        "win",
        "query",
        "row",
        "sortkey",
        "symbol",
        "winspec",
        "frame",
        "bound",
        "keyLabel",
        "valueLabel",
        "name",
    ];
    let mut bad = Vec::new();
    for line in &section {
        let Some((_, rest)) = line.split_once(" : ") else {
            continue;
        };
        let sig = rest.split("  ").next().unwrap_or(rest);
        let sig = sig.split(" = ").next().unwrap_or(sig);
        for (i, ch) in sig.char_indices() {
            if !ch.is_ascii_lowercase() {
                continue;
            }
            let before = sig[..i].chars().next_back();
            let after = sig[i + 1..].chars().next();
            let standalone = before
                .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '?' && c != '`')
                && after.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '`');
            if !standalone {
                continue;
            }
            let word: String = sig[i..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if word.len() == 1 && !real.contains(&word.as_str()) {
                bad.push(line.trim().to_string());
                break;
            }
        }
    }
    assert!(
        bad.is_empty(),
        "these catalog lines write a type variable without the `?` sigil, so the \
         signature cannot be pasted into a file: {bad:#?}"
    );
}

/// Every document in `docs/` itself is reachable from somewhere a reader starts.
///
/// `docs/` had sixteen files and no way to tell which four were current: the dated
/// research records sat beside the documents that track the code, and nothing linked to
/// seven of them. They live in `docs/research/` now, and this keeps the top level from
/// growing back — a new document that nothing links to is a document nobody reads.
///
/// Reachable means named by a link from `README.md`, `CONTRIBUTING.md`, `CAGARA.md`, or
/// any other document under `docs/`. `research/README.md` covers its own directory, so
/// the records are reachable through it rather than needing seven separate links.
#[test]
fn every_document_in_docs_is_reachable() {
    let root = repo_root();
    // Everything a reader might start from, including the docs themselves.
    let mut sources: Vec<PathBuf> = ["README.md", "CONTRIBUTING.md", "CAGARA.md"]
        .iter()
        .map(|f| root.join(f))
        .filter(|p| p.exists())
        .collect();
    sources.extend(documents());

    let mut named: Vec<String> = Vec::new();
    for path in &sources {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        // Markdown links and plain mentions both count: this is about whether a
        // reader can find the file, not about link syntax.
        for (i, _) in src.match_indices(".md") {
            let start = src[..i].rfind(|c: char| c.is_whitespace() || c == '(' || c == '"');
            let from = start.map_or(0, |s| s + 1);
            named.push(src[from..i].to_string());
        }
    }

    let mut unreachable = Vec::new();
    for path in documents() {
        let name = doc_name(&path);
        // Only the top level is policed; `docs/research/` is covered by its own README.
        if name.contains('/') {
            continue;
        }
        let stem = name.trim_end_matches(".md");
        if !named
            .iter()
            .any(|n| n.ends_with(&name) || n.ends_with(stem))
        {
            unreachable.push(name);
        }
    }
    assert!(
        unreachable.is_empty(),
        "these documents are in `docs/` and nothing links to them, so a reader will \
         not find them: {unreachable:?}\nLink them from README.md or another document, \
         or move them to docs/research/ if they are a dated record rather than \
         documentation."
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
        let name = doc_name(&path);
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

/// A claim that a *probe* settled stays settled.
///
/// The rules below were each checked against the compiler by hand — by writing the
/// program and reading the type or the SQL it produced — and each one is the kind of
/// sentence a document drifts away from silently, because nothing about editing the
/// prose makes it fail. When one of them changes, this test fails and names the
/// document that says otherwise.
///
/// The list is short on purpose. It holds the claims a reader would *act* on: a
/// statement about what the language accepts, that someone would write a query around.
/// A document that merely describes a design is not here, because a design can change
/// without any query changing.
const SETTLED: &[(&str, &str)] = &[
    // `docs/CORE.md`, `CAGARA.md` §0/§5.8/§10, `docs/LEARN.md` §11 and
    // `docs/ARCHITECTURE.md` all say a set operation matches its two sides **by
    // name**, so the orders may differ, and answers in the left operand's order.
    (
        "the columns of a set operation are matched by name",
        "docs/CORE.md, CAGARA.md, docs/LEARN.md, docs/ARCHITECTURE.md",
    ),
    // `docs/CORE.md` and `docs/SQL-DIALECTS.md`: the three frame units, and that two
    // targets refuse some of them.
    (
        "`rows`, `range` and `groups` are three different frame units",
        "docs/CORE.md, docs/SQL-DIALECTS.md",
    ),
    // `docs/NULLABILITY.md`: which aggregates return `maybe`.
    (
        "`sum`/`avg`/`min`/`max` return `maybe`; the counts return `int`",
        "docs/NULLABILITY.md",
    ),
];

/// The settled claims, probed against the compiler.
///
/// Each probe is a program and an expected fragment of what the compiler says about it.
/// A claim that stops holding fails here, and the message names the documents that
/// would then be wrong — because the failure is not in this file, it is in them.
#[test]
fn the_settled_claims_still_hold() {
    let dir = tempfile::Builder::new()
        .prefix("cagara-claims-")
        .tempdir()
        .expect("temp dir");

    // A program, the mode that answers the question, and what it must say about `q`.
    let probes: &[(&str, &[&str], &str, &str)] = &[
        // A set operation matches by name and answers in the left's order: `a` lists
        // `id, n` and `b` lists `n, id`, and the answer is `a`'s.
        // The claim has two halves and they fail in different phases: the *type* says
        // `a`'s order (the checker's rule) and the *SQL* has both sides in `a`'s order
        // (the arena's). `--types` alone would pass with the arena refusing the query,
        // which is why this probes both.
        (
            "a : query { id = int, n = string } = table \"m\" \"a\"\n\
             b : query { n = string, id = int } = table \"m\" \"b\"\n\
             q = a & union b\n",
            &["--types"],
            "q : query { id = int, n = string }",
            "the columns of a set operation are matched by name",
        ),
        (
            "a : query { id = int, n = string } = table \"m\" \"a\"\n\
             b : query { n = string, id = int } = table \"m\" \"b\"\n\
             q = a & union b\n",
            &[],
            "SELECT id, n FROM m.a UNION SELECT id, n FROM m.b",
            "the columns of a set operation are matched by name",
        ),
        // The three frame units exist and each reaches the SQL with its own keyword.
        (
            "u : query { id = int } = table \"m\" \"u\"\n\
             q = u & select { s = over { order = [asc .id], frame = rows (preceding 1) \
             (following 1) } (sum .id) }\n",
            // No `--types`: the claim is about the emitted clause, which is the SQL
            // mode's output.
            &[],
            "ROWS BETWEEN",
            "`rows`, `range` and `groups` are three different frame units",
        ),
        // The aggregate nullability table.
        (
            "u : query { id = int, s = maybe int } = table \"m\" \"u\"\n\
             q = u & agg { a = sum .s, b = count, c = countOf .s }\n",
            &["--types"],
            "q : query { a = maybe int, b = int, c = int }",
            "`sum`/`avg`/`min`/`max` return `maybe`; the counts return `int`",
        ),
    ];

    let mut failures = Vec::new();
    for (i, (src, flags, want, claim)) in probes.iter().enumerate() {
        let file = dir.path().join(format!("probe{i}.cagara"));
        std::fs::write(&file, src).expect("write probe");
        let out = Command::new(env!("CARGO_BIN_EXE_cagara"))
            .arg(&file)
            .args(["--only", "q"])
            .args(*flags)
            .output()
            .expect("run cagara");
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !said.contains(want) {
            let docs = SETTLED
                .iter()
                .find(|(c, _)| c == claim)
                .map(|(_, d)| *d)
                .unwrap_or("(not listed)");
            failures.push(format!(
                "`{claim}` no longer holds, so {docs} are wrong: expected {want:?}, \
                 got {said:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
