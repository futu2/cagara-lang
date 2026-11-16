//! Finding the sources a command was pointed at.
//!
//! One walk, so `cagara check` and `cagara fmt` cannot disagree about what a
//! directory means.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Every `*.cagara` file under `path`, in a stable order. A file path is itself.
///
/// Shared by `cagara check` and `cagara fmt`, which have the same problem: a user
/// points at a tree, and the tool has to find the sources in it exactly once. `fmt`
/// used to read its argument directly, so `cagara fmt examples/` failed with
/// `Is a directory` while `cagara check examples/` worked — a difference with no
/// reason behind it.
pub(crate) fn sources_under(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !path.is_dir() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    // Each *file* once, however many paths reach it.
    //
    // A symlink is followed, so the same file can be reached many times: `ln -s .
    // self` inside a directory makes every level of the walk see it again, and the
    // walk stops only because the OS refuses to resolve more than 40 symlinks in one
    // path. Measured on this tree: 41 entries for one six-byte file, 41 workspaces
    // compiled, 1.6 s instead of 0.04 s. A symlink to a sibling directory is the
    // quieter version — the file is checked twice, and only the diagnostic dedupe
    // hid it.
    //
    // The key is the canonical path, and the path *kept* is the first one the sorted
    // walk reached, so what is printed is a path the user typed or one under it.
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // A directory already walked needs no second visit, which is what bounds a
        // loop to one pass instead of forty.
        if !seen_dirs.insert(std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                // A dot directory is a build artifact or an editor's cache,
                // not part of the tree being checked.
                if entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                stack.push(entry);
            } else if entry.extension().is_some_and(|e| e == "cagara") {
                let key = std::fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
                if seen_files.insert(key) {
                    out.push(entry);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}
