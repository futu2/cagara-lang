//! Doc comments: text that belongs to a definition rather than to the file.
//!
//! A `#` comment is prose for whoever reads the file. Two prefixed forms of it
//! are documentation instead, and are attached to something:
//!
//! - `##` on the line(s) directly above a top-level definition documents *that
//!   definition*. The block ends at the first line that is not a `##` line, so
//!   a blank line or an ordinary `#` comment separates a doc block from a
//!   definition and leaves it unattached.
//! - `#!` above the first item documents the *file*. The last such run wins, so
//!   a file can keep its prose header and put its own documentation below it.
//!
//! Nothing about the grammar changes: the lexer still produces one `Comment`
//! token, the formatter still prints it verbatim, and a file with no `##` lines
//! means exactly what it meant before. What the markers add is a name for text
//! that already had a place, so more than one reader can use it — `cagara doc`
//! prints a reference, the language server answers hover from it, and the
//! comments stay next to the definitions they describe.
//!
//! Both markers require the line they mark: a `##` at the end of a line of code
//! is a trailing comment, not documentation of the next definition.

use crate::ast::Module;
use std::ops::Range;

/// What makes a line documentation of the definition below it.
pub const ITEM_MARKER: &str = "##";

/// What makes a line documentation of the file.
pub const MODULE_MARKER: &str = "#!";

/// The documentation of the item whose first token is at `offset`: the block
/// of `##` lines directly above the line `offset` is on.
///
/// `None` when there is no such block, which is the ordinary case.
///
/// Read from the text rather than stored in the AST, because documentation is a
/// property of the source and not of the program: the AST is what the checker
/// reasons about, and a doc block reaches its definition through the
/// definition's span — which is the whole of the connection, and needs no field
/// to carry it. Every caller that can ask the question already holds the text
/// (a loaded module keeps it for spans and diagnostics).
///
/// The scan looks at the lines above the item and stops at the first that is
/// not a doc line, so it costs the size of the block rather than the size of
/// the file.
pub fn item_doc(src: &str, offset: usize) -> Option<String> {
    let mut block: Vec<&str> = Vec::new();
    let mut end = offset.min(src.len());
    while let Some((start, line)) = line_above(src, end) {
        let Some(text) = doc_text(line, ITEM_MARKER) else {
            break;
        };
        block.push(text);
        end = start;
    }
    block.reverse();
    join(&block)
}

/// The documentation of a whole file: the last `#!` run above its first item.
///
/// Read from the module rather than from the text alone, because "above the
/// first item" is a fact about the program: a file with no items has no
/// boundary to be above, so its whole text is the extent.
pub fn module_doc(src: &str, module: &Module) -> Option<String> {
    let first = module
        .imports
        .iter()
        .map(|i| i.span.start)
        .chain(module.defs.iter().map(|d| d.span.start))
        .min()
        .map_or(src.len(), |start| start as usize);
    Docs::new(src).module(first)
}

/// The text of the line ending just before `offset`, with the offset that line
/// starts at. `None` at the start of the file.
///
/// `offset` is expected at a line break or at a line start — every caller passes
/// the start of a definition or the start of a line it just walked up to.
fn line_above(src: &str, offset: usize) -> Option<(usize, &str)> {
    let before = src.get(..offset)?;
    // Exactly one line break, because a blank line is a line: skipping every
    // break would walk over an empty line and attach a doc block across it.
    // A break is `\n`, `\r\n` or a bare `\r`, the same rule the lexer and the
    // parser's layout use.
    let end = if before.ends_with("\r\n") {
        before.len() - 2
    } else if before.ends_with(['\n', '\r']) {
        before.len() - 1
    } else {
        before.len()
    };
    let start = before
        .get(..end)?
        .rfind(['\n', '\r'])
        .map_or(0, |at| at + 1);
    Some((start, src.get(start..end)?))
}

/// A file's line index, for questions that are about the whole file rather than
/// about one definition: a file's own `#!` block, and the line number a warning
/// names.
///
/// Built once per file by callers that ask several of these questions.
#[derive(Debug, Clone)]
pub struct Docs<'a> {
    /// `(line text, byte offset of its first character)`, in source order.
    lines: Vec<(&'a str, usize)>,
    len: usize,
}

impl<'a> Docs<'a> {
    pub fn new(src: &'a str) -> Self {
        Docs {
            lines: lines(src),
            len: src.len(),
        }
    }

    /// The documentation of the file: the last `#!` run that ends before
    /// `before`, which callers pass as the offset of the first item (or the end
    /// of the file when there is no item).
    pub fn module(&self, before: usize) -> Option<String> {
        let end = self.line_of(before);
        let mut last: Option<Range<usize>> = None;
        let mut i = 0;
        while i < end {
            if doc_text(self.lines.get(i)?.0, MODULE_MARKER).is_none() {
                i += 1;
                continue;
            }
            let start = i;
            while i < end && doc_text(self.lines.get(i)?.0, MODULE_MARKER).is_some() {
                i += 1;
            }
            last = Some(start..i);
        }
        let run = last?;
        let block: Vec<&str> = run
            .filter_map(|n| self.lines.get(n).map(|l| l.0))
            .filter_map(|l| doc_text(l, MODULE_MARKER))
            .collect();
        join(&block)
    }

    /// The 1-based line number of `offset`, for a warning that names a place.
    pub fn line(&self, offset: usize) -> usize {
        self.line_of(offset) + 1
    }

    /// Index into [`Docs::lines`] of the line containing `offset`.
    fn line_of(&self, offset: usize) -> usize {
        let offset = offset.min(self.len);
        // The last line that starts at or before `offset`. `partition_point`
        // rather than a linear scan keeps this O(log lines).
        let after = self.lines.partition_point(|(_, start)| *start <= offset);
        after.saturating_sub(1)
    }
}

/// The documentation text of `line`, if the line is a `<marker>` line.
///
/// The marker may be indented, because a definition may be indented in a
/// context this module does not see (a test fixture, a fragment pasted into an
/// editor). One space after the marker is the conventional separator and is
/// dropped, so `##  x` keeps one leading space; the rest is kept as written.
fn doc_text<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(marker)?;
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    Some(rest.trim_end())
}

/// A doc block as one string, without the blank lines that pad it.
fn join(block: &[&str]) -> Option<String> {
    let first = block.iter().position(|l| !l.is_empty())?;
    let last = block.iter().rposition(|l| !l.is_empty())?;
    let body = block.get(first..=last)?;
    Some(body.join("\n"))
}

/// Split into lines, treating `\n`, `\r\n` and a bare `\r` as one line break —
/// the same rule the lexer and the parser's layout use, so a CR-only file
/// documents its definitions too.
fn lines(src: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'\n') => {
                out.push((&src[start..i], start));
                i += 1;
                start = i;
            }
            Some(b'\r') => {
                out.push((&src[start..i], start));
                i += 1;
                if bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                start = i;
            }
            Some(_) => i += 1,
            None => break,
        }
    }
    out.push((&src[start..], start));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// The offset of `needle` in `src`, which is how a test names an item.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle")
    }

    /// A source and the module it lowers to, for the whole-file questions.
    fn lower(src: &str) -> (String, crate::ast::Module) {
        let (module, errors) = crate::ast::lower_source(src);
        assert!(errors.is_empty(), "{errors:?}");
        (src.to_string(), module)
    }

    #[test]
    fn a_doc_block_above_a_definition_belongs_to_it() {
        let src = "## The entry point.\n##\n## It is a query.\nmain = q\n";
        assert_eq!(
            item_doc(src, at(src, "main")).as_deref(),
            Some("The entry point.\n\nIt is a query.")
        );
    }

    /// Prose above a definition is not documentation of it: the marker is what
    /// makes the text attached, and a `#` comment is the file's own prose.
    #[test]
    fn an_ordinary_comment_is_not_attached() {
        let src = "# Just prose.\nmain = q\n";
        assert_eq!(item_doc(src, at(src, "main")), None);
    }

    /// A blank line ends the block, so an implementation note left above a
    /// definition is not silently presented as its documentation.
    #[test]
    fn a_blank_line_detaches_the_block() {
        let src = "## Not this one.\n\nmain = q\n";
        assert_eq!(item_doc(src, at(src, "main")), None);
    }

    #[test]
    fn only_the_run_directly_above_is_documentation() {
        let src = "## Old.\n# Separator.\n## New.\nmain = q\n";
        assert_eq!(item_doc(src, at(src, "main")).as_deref(), Some("New."));
    }

    /// A trailing `##` documents nothing: it is on a line of code, and the
    /// definition below it was not what the writer was describing.
    #[test]
    fn a_trailing_doc_marker_attaches_to_nothing() {
        let src = "helper = 1 ## about helper\nmain = helper\n";
        assert_eq!(item_doc(src, at(src, "main")), None);
        assert_eq!(item_doc(src, at(src, "helper")), None);
    }

    /// Doc blocks travel with CRLF and CR-only files, and their text carries no
    /// carriage return into the reference.
    #[test]
    fn doc_blocks_survive_every_line_break() {
        for sep in ["\n", "\r\n", "\r"] {
            let src = format!("## One.{sep}## Two.{sep}main = q{sep}");
            assert_eq!(
                item_doc(&src, at(&src, "main")).as_deref(),
                Some("One.\nTwo."),
                "{sep:?}"
            );
        }
    }

    #[test]
    fn a_file_doc_is_the_last_run_before_the_first_item() {
        let src = "# A prose header.\n#! The file.\nimport \"x.cagara\"\nmain = q\n";
        assert_eq!(
            Docs::new(src).module(at(src, "import")).as_deref(),
            Some("The file.")
        );
    }

    /// `#!` after the first item documents nothing: a file's documentation
    /// describes the whole file, so it cannot come after half of it.
    #[test]
    fn a_file_doc_below_an_item_is_ignored() {
        let src = "main = q\n#! Late.\n";
        assert_eq!(Docs::new(src).module(at(src, "main")), None);
    }

    /// [`module_doc`] takes the module, because "above the first item" is a
    /// fact about the program: an import and a definition both start the file's
    /// item list, and a file with no items has no boundary at all.
    #[test]
    fn a_file_doc_is_found_above_whichever_item_comes_first() {
        let (src, module) = lower("import \"x.cagara\"\nmain = q\n");
        assert_eq!(module_doc(&src, &module), None);
        let (src, module) = lower("#! Above the import.\nimport \"x.cagara\"\nmain = q\n");
        assert_eq!(
            module_doc(&src, &module).as_deref(),
            Some("Above the import.")
        );
        let (src, module) = lower("#! Above the definition.\nmain = q\n");
        assert_eq!(
            module_doc(&src, &module).as_deref(),
            Some("Above the definition.")
        );
    }

    /// A file with no items at all is all header, so its `#!` block is its
    /// documentation.
    #[test]
    fn a_file_with_no_items_can_still_be_documented() {
        let (src, module) = lower("#! Only prose.\n");
        assert_eq!(module_doc(&src, &module).as_deref(), Some("Only prose."));
    }

    #[test]
    fn a_prefix_only_doc_marker_is_an_empty_doc() {
        // `##` with no text is a separator inside a block, but a lone one is
        // not documentation worth reporting either.
        let src = "##\nmain = q\n";
        assert_eq!(item_doc(src, at(src, "main")), None);
        let src = "##  leading space kept\nmain = q\n";
        assert_eq!(
            item_doc(src, at(src, "main")).as_deref(),
            Some(" leading space kept")
        );
    }

    #[test]
    fn line_numbers_are_one_based() {
        let src = "x = 1\n\ny = 2\n";
        let docs = Docs::new(src);
        assert_eq!(docs.line(at(src, "x")), 1);
        assert_eq!(docs.line(at(src, "y")), 3);
        assert_eq!(docs.line(src.len()), 4);
    }
}
