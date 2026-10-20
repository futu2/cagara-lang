//! The reference documents, compiled.
//!
//! `CAGARA.md` and the README both show complete programs. A prose reference
//! can drift from the compiler silently; a *compiled* one cannot. These tests
//! extract the tagged blocks and run the real binary over each one, so a stale
//! example fails CI rather than a reader.
//!
//! The tests are deliberately about the binary's exit code, and for the
//! README also about the SQL it prints: the blocks are examples of valid
//! programs, and a block whose SQL no longer matches the prose is just as
//! stale as one that does not compile.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Every ```` ```cagara ```` block in `src`, in document order.
///
/// The fence is matched at line starts, and the closing fence is the next line
/// that starts with three backticks. That is enough for these files: no example
/// contains a nested fence.
fn cagara_blocks(src: &str) -> Vec<String> {
    fenced_blocks(src, "cagara")
}

/// Every ```` ```sql ```` block in `src`, in document order.
fn sql_blocks(src: &str) -> Vec<String> {
    fenced_blocks(src, "sql")
}

fn fenced_blocks(src: &str, language: &str) -> Vec<String> {
    let open = format!("```{language}");
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in src.lines() {
        match current.as_mut() {
            Some(body) if line.starts_with("```") => {
                blocks.push(body.join("\n"));
                current = None;
            }
            Some(body) => body.push(line),
            None if line == open => current = Some(Vec::new()),
            None => {}
        }
    }
    assert!(
        current.is_none(),
        "a ```{language} block is unterminated in the source"
    );
    blocks
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Compile every block as its own file, with a message naming the document and
/// block so a failure is actionable.
fn compile_blocks(label: &str, blocks: &[String]) {
    let dir = tempfile::Builder::new()
        .prefix("cagara-doc-blocks-")
        .tempdir()
        .expect("create temp dir");

    for (i, block) in blocks.iter().enumerate() {
        let file = dir.path().join(format!("block-{i:02}.cagara"));
        std::fs::write(&file, block).expect("write block");
        let out = run(&file);
        assert!(
            out.status.success(),
            "{label} block {} does not compile:\n{}\n--- source ---\n{}",
            i + 1,
            String::from_utf8_lossy(&out.stderr),
            block
        );
    }
}

fn run(file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cagara"))
        .arg(file)
        .output()
        .expect("run cagara")
}

/// Whitespace-insensitive SQL comparison: the README breaks the statement over
/// several lines, and the compiler prints it on one.
fn squash(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_cagara_block_in_the_agent_reference_compiles() {
    let path = repo_root().join("CAGARA.md");
    let src = read(&path);
    let blocks = cagara_blocks(&src);
    assert!(
        blocks.len() >= 30,
        "expected many examples in {}, found {}",
        path.display(),
        blocks.len()
    );
    compile_blocks("CAGARA.md", &blocks);
}

#[test]
fn the_readme_example_compiles_and_matches_its_sql() {
    let src = read(&repo_root().join("README.md"));
    let blocks = cagara_blocks(&src);
    assert_eq!(
        blocks.len(),
        1,
        "the README should show exactly one complete program"
    );
    compile_blocks("README.md", &blocks);

    // Write the one block and compare the compiler's statement with the SQL
    // the README promises. The banner is not part of the statement.
    let dir = tempfile::Builder::new()
        .prefix("cagara-readme-")
        .tempdir()
        .expect("create temp dir");
    let file = dir.path().join("readme.cagara");
    std::fs::write(&file, &blocks[0]).expect("write README example");
    let out = run(&file);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let actual = stdout
        .lines()
        .skip_while(|line| line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let expected = sql_blocks(&src)
        .into_iter()
        .next()
        .expect("the README should show the SQL it produces");
    assert_eq!(
        squash(&actual),
        squash(&expected),
        "README SQL is stale; the compiler now prints:\n{stdout}"
    );
}

/// Every place that names the version must agree with the binary.
///
/// The release workflow checks the tag against `Cargo.toml` and the VS Code
/// package only. These files are copied into releases or read by users, so a
/// stale number is visible even though no build fails.
#[test]
fn version_references_agree() {
    let version = env!("CARGO_PKG_VERSION");
    let root = repo_root();

    let flake = read(&root.join("flake.nix"));
    assert!(
        flake.contains(&format!("version = \"{version}\";")),
        "flake.nix does not name version {version}"
    );

    let cagara = read(&root.join("CAGARA.md"));
    assert!(
        cagara.contains(&format!("**Version {version}.**")),
        "CAGARA.md does not name version {version}"
    );
    assert!(
        cagara.contains(&format!("You are writing Cagara {version}.")),
        "the CAGARA.md prompt skeleton does not name version {version}"
    );

    let package = read(&root.join("editors/vscode/package.json"));
    assert!(
        package.contains(&format!("\"version\": \"{version}\"")),
        "the VS Code package does not name version {version}"
    );

    let vscode_readme = read(&root.join("editors/vscode/README.md"));
    let named = vscode_readme
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')))
        .filter(|token| token.starts_with("cagara-vscode-v"));
    let mut count = 0;
    let expected = format!("cagara-vscode-v{version}");
    for token in named {
        count += 1;
        let rest = token.strip_prefix(&expected).unwrap_or(token);
        assert!(
            rest.is_empty() || rest.starts_with('.') || rest.starts_with('-'),
            "the VS Code README names {token}, not version {version}"
        );
    }
    assert!(
        count > 0,
        "the VS Code README should name at least one release package"
    );
}

/// Two small unit checks keep the extractor itself from silently returning
/// nothing (which would make the tests above vacuous).
#[test]
fn block_extraction_finds_only_tagged_blocks() {
    let src = "```text\nnot cagara\n```\n\n```cagara\nmain = 1\n```\n";
    assert_eq!(cagara_blocks(src), vec!["main = 1".to_string()]);
}

#[test]
fn block_extraction_rejects_an_unterminated_block() {
    let src = "```cagara\nmain = 1\n";
    let result = std::panic::catch_unwind(|| cagara_blocks(src));
    assert!(result.is_err(), "an unterminated block must be reported");
}
