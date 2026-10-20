//! The agent reference, compiled.
//!
//! `CAGARA.md` makes a strong promise: every block tagged `cagara` is a
//! complete file that compiles. A prose reference can drift from the compiler
//! silently; a *compiled* one cannot. This test extracts each such block and
//! runs the real binary over it, so a stale example fails CI rather than an
//! agent.
//!
//! The test is deliberately about the binary's exit code and not about its
//! SQL: the blocks are examples of *valid programs*, and `main` may be absent
//! (a library) or emit a statement whose exact text is not the point.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every ```` ```cagara ```` block in `src`, in document order.
///
/// The fence is matched at line starts, and the closing fence is the next line
/// that starts with three backticks. That is enough for this file: no example
/// contains a nested fence.
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
            None if line == "```cagara" => current = Some(Vec::new()),
            None => {}
        }
    }
    assert!(
        current.is_none(),
        "CAGARA.md has an unterminated ```cagara block"
    );
    blocks
}

fn reference_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("CAGARA.md")
}

#[test]
fn every_cagara_block_in_the_agent_reference_compiles() {
    let path = reference_path();
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let blocks = cagara_blocks(&src);
    assert!(
        blocks.len() >= 30,
        "expected many examples in {}, found {}",
        path.display(),
        blocks.len()
    );

    let dir = tempfile::Builder::new()
        .prefix("cagara-doc-blocks-")
        .tempdir()
        .expect("create temp dir");

    for (i, block) in blocks.iter().enumerate() {
        let file = dir.path().join(format!("block-{i:02}.cagara"));
        std::fs::write(&file, block).expect("write block");
        let out = Command::new(env!("CARGO_BIN_EXE_cagara"))
            .arg(&file)
            .output()
            .expect("run cagara");
        assert!(
            out.status.success(),
            "CAGARA.md block {} does not compile:\n{}\n--- source ---\n{}",
            i + 1,
            String::from_utf8_lossy(&out.stderr),
            block
        );
    }
}

/// Two small unit checks keep the extractor itself from silently returning
/// nothing (which would make the test above vacuous).
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
