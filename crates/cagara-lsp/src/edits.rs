//! Turning a whole-file formatting result into the edits that produce it.
//!
//! `cagara_fmt` formats a file as a unit, and the language server used to hand
//! the client one edit spanning line 0 to the end of the file. An editor pays
//! for that edit in full: every token, fold and bracket guide is invalidated,
//! the undo entry is a copy of the whole file, the cursor and selection move,
//! and a client that mirrors the buffer elsewhere — a vim or neovim extension —
//! is handed every line to push back. Formatting one definition of a thousand
//! therefore cost the editor a whole-file change, which is the one thing
//! formatting a single definition should never cost.
//!
//! The edits here cover only the lines that differ. They are found with a
//! line-level patience diff: a line that occurs exactly once in both texts is
//! one the formatter neither duplicated nor dropped, so it anchors the two and
//! the gaps between anchors are compared again. A re-laid-out definition
//! becomes one small edit, and the definitions around it are not touched at
//! all.

use std::collections::HashMap;

use lsp_types::{Position, Range, TextEdit};

/// Edits that turn `old` into `new`, one per run of changed lines.
///
/// Ranges are in `old` and ascending, so a client applies them back to front,
/// and applying all of them reproduces `new`: outside the hunks both texts have
/// the same lines, and a hunk's `new_text` is the run of new lines that
/// replaced its old ones.
pub fn between(old: &str, new: &str) -> Vec<TextEdit> {
    let old_lines = split(old);
    let new_lines = split(new);
    let mut hunks = Vec::new();
    compare(
        &old_lines,
        &new_lines,
        0,
        old_lines.len(),
        0,
        new_lines.len(),
        &mut hunks,
    );
    let end_of_old = end_position(&old_lines);
    hunks
        .into_iter()
        .map(|h| TextEdit {
            range: Range {
                start: Position {
                    line: h.old_start as u32,
                    character: 0,
                },
                // A hunk that runs to the end of the old text ends where the
                // text ends, which is a column past the last character when
                // the file does not end with a newline.
                end: if h.old_end == old_lines.len() {
                    Position {
                        line: end_of_old.0,
                        character: end_of_old.1,
                    }
                } else {
                    Position {
                        line: h.old_end as u32,
                        character: 0,
                    }
                },
            },
            new_text: new_lines[h.new_start..h.new_end].concat(),
        })
        .collect()
}

/// One run of lines replaced: old lines `old_start..old_end` become new lines
/// `new_start..new_end`. Line indices, counted from 0.
struct Hunk {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// The lines of `text`, each keeping its newline, so concatenating a run of
/// them reproduces exactly the bytes between two line boundaries.
fn split(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The position just past the last character: the start of the line after the
/// last one when the text ends with a newline, otherwise the column after the
/// last character.
fn end_position(lines: &[&str]) -> (u32, u32) {
    match lines.last() {
        None => (0, 0),
        Some(last) if last.ends_with('\n') => (lines.len() as u32, 0),
        // Characters, not bytes: a range in the middle of a multi-byte
        // character is not a position any client will accept.
        Some(last) => ((lines.len() - 1) as u32, last.chars().count() as u32),
    }
}

/// Compare `old[a0..a1]` with `new[b0..b1]`, recording the runs that differ.
///
/// The common prefix and suffix are dropped first, which is what keeps an edit
/// at one end of a file from being reported as a change to the whole file.
/// What remains is split at anchor lines and each gap is compared again.
fn compare(
    old: &[&str],
    new: &[&str],
    mut a0: usize,
    mut a1: usize,
    mut b0: usize,
    mut b1: usize,
    out: &mut Vec<Hunk>,
) {
    while a0 < a1 && b0 < b1 && old[a0] == new[b0] {
        a0 += 1;
        b0 += 1;
    }
    while a1 > a0 && b1 > b0 && old[a1 - 1] == new[b1 - 1] {
        a1 -= 1;
        b1 -= 1;
    }
    if a0 == a1 && b0 == b1 {
        return;
    }
    // Lines only on one side: the whole rest is one replacement.
    if a0 == a1 || b0 == b1 {
        out.push(Hunk {
            old_start: a0,
            old_end: a1,
            new_start: b0,
            new_end: b1,
        });
        return;
    }
    let anchors = anchors(&old[a0..a1], &new[b0..b1]);
    if anchors.is_empty() {
        out.push(Hunk {
            old_start: a0,
            old_end: a1,
            new_start: b0,
            new_end: b1,
        });
        return;
    }
    let (mut next_a, mut next_b) = (a0, b0);
    for (a, b) in anchors {
        compare(old, new, next_a, a0 + a, next_b, b0 + b, out);
        next_a = a0 + a + 1;
        next_b = b0 + b + 1;
    }
    compare(old, new, next_a, a1, next_b, b1, out);
}

/// Pairs of equal lines that occur exactly once in each range, in the order
/// they appear in `old`, keeping the longest run that is also in order in
/// `new`.
///
/// Uniqueness is the whole point: a line that appears once on each side is one
/// the formatter kept, so the text between two such lines can be compared on
/// its own and a repeated line (`end`, `}`, a blank) cannot be paired up wrong
/// and make two distant edits look like one.
fn anchors(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let mut counts: HashMap<&str, (usize, Option<usize>, Option<usize>)> = HashMap::new();
    for (i, line) in old.iter().enumerate() {
        let entry = counts.entry(line).or_insert((0, None, None));
        entry.0 += 1;
        entry.1.get_or_insert(i);
    }
    for (i, line) in new.iter().enumerate() {
        let entry = counts.entry(line).or_insert((0, None, None));
        entry.0 += 1;
        entry.2.get_or_insert(i);
    }
    let mut pairs: Vec<(usize, usize)> = counts
        .values()
        .filter(|(count, ..)| *count == 2)
        .filter_map(|(_, a, b)| Some(((*a)?, (*b)?)))
        .collect();
    pairs.sort_unstable();
    increasing(&pairs)
}

/// The longest prefix-order-preserving subsequence of `pairs` that is also
/// ordered by its second element: patience sorting, so O(n log n).
fn increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; pairs.len()];
    for (i, pair) in pairs.iter().enumerate() {
        let at = tails.partition_point(|&t| pairs[t].1 < pair.1);
        if at > 0 {
            previous[i] = Some(tails[at - 1]);
        }
        if at == tails.len() {
            tails.push(i);
        } else {
            tails[at] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut current = tails.last().copied();
    while let Some(i) = current {
        out.push(pairs[i]);
        current = previous[i];
    }
    out.reverse();
    out
}

/// Splicing edits back into the text, the way a client does.
///
/// Kept next to the code that mints the ranges, so the round trip is checked
/// against the same reading of a position that produced them.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn apply(old: &str, edits: &[TextEdit]) -> String {
        let starts = line_starts(old);
        let mut out = old.to_string();
        // Back to front: each range is in the old text, so applying a later
        // one first leaves the earlier offsets valid.
        for edit in edits.iter().rev() {
            let start = offset(&starts, old, edit.range.start);
            let end = offset(&starts, old, edit.range.end);
            out.replace_range(start..end, &edit.new_text);
        }
        out
    }

    fn line_starts(text: &str) -> Vec<usize> {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        starts
    }

    fn offset(starts: &[usize], text: &str, p: Position) -> usize {
        let base = starts[p.line as usize];
        base + text[base..]
            .chars()
            .take(p.character as usize)
            .map(char::len_utf8)
            .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::apply;
    use super::*;

    /// An edit as `(start_line, end_line, text)`, so a failure says what the
    /// edits were rather than just that two structs differ. Every edit is
    /// checked to start at a line boundary on the way; the one exception, an
    /// edit ending past the last character, has its own test.
    fn shape(edits: &[TextEdit]) -> Vec<(u32, u32, String)> {
        edits
            .iter()
            .map(|e| {
                assert_eq!(e.range.start.character, 0, "an edit starts at a line");
                assert_eq!(e.range.end.character, 0, "an edit ends at a line");
                (e.range.start.line, e.range.end.line, e.new_text.clone())
            })
            .collect()
    }

    #[test]
    fn identical_text_needs_no_edits() {
        assert!(between("a\nb\n", "a\nb\n").is_empty());
        assert!(between("", "").is_empty());
    }

    #[test]
    fn a_changed_line_is_one_edit() {
        let edits = between("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(shape(&edits), vec![(1, 2, "B\n".to_string())]);
    }

    #[test]
    fn a_line_that_becomes_three_is_one_edit() {
        let edits = between("a\nq = x & y\nc\n", "a\nq = x\n  & y\nc\n");
        assert_eq!(shape(&edits), vec![(1, 2, "q = x\n  & y\n".to_string())]);
    }

    #[test]
    fn changed_lines_that_are_far_apart_stay_apart() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nB\nc\nd\ne\nf\nG\nh\n";
        let edits = between(old, new);
        assert_eq!(
            shape(&edits),
            vec![(1, 2, "B\n".to_string()), (6, 7, "G\n".to_string())]
        );
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn a_repeated_line_does_not_pair_up_wrongly() {
        // `}` appears three times on each side, so it anchors nothing; the one
        // changed line between the second and third is still found on its own.
        let old = "x\n}\ny\n}\nz\n}\n";
        let new = "x\n}\ny\n}\nZ\n}\n";
        let edits = between(old, new);
        assert_eq!(shape(&edits), vec![(4, 5, "Z\n".to_string())]);
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn appending_uses_the_document_end() {
        let edits = between("a\nb\n", "a\nb\nc\n");
        assert_eq!(shape(&edits), vec![(2, 2, "c\n".to_string())]);
        assert_eq!(apply("a\nb\n", &edits), "a\nb\nc\n");
    }

    #[test]
    fn a_file_without_a_final_newline_ends_at_its_last_column() {
        let edits = between("a\nb", "a\nbc");
        let [edit] = edits.as_slice() else {
            panic!("{edits:?}")
        };
        assert_eq!(
            edit.range.start,
            Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            edit.range.end,
            Position {
                line: 1,
                character: 1
            }
        );
        assert_eq!(apply("a\nb", &edits), "a\nbc");
    }

    #[test]
    fn the_end_of_a_text_is_chars_not_bytes() {
        // The last line holds a two-byte character and no newline: the end
        // position is column 2, not byte 3.
        let edits = between("a\né", "a\néé");
        let [edit] = edits.as_slice() else {
            panic!("{edits:?}")
        };
        assert_eq!(
            edit.range.end,
            Position {
                line: 1,
                character: 1
            }
        );
        assert_eq!(apply("a\né", &edits), "a\néé");
    }

    #[test]
    fn an_empty_file_becomes_the_formatted_one() {
        let edits = between("", "main = 1\n");
        assert_eq!(shape(&edits), vec![(0, 0, "main = 1\n".to_string())]);
        assert_eq!(apply("", &edits), "main = 1\n");
    }

    #[test]
    fn edits_reproduce_what_the_formatter_wrote() {
        // Whole-file layout changes, a comment moving, and a definition left
        // alone, which is the mix a real format request sees.
        let old = "## A table.\nusers : query { id = int } = table \"p\" \"users\"\n\n\
                   q = users & where .id > 1 & select { x = .id }\n\n\
                   already_formatted = 1\n";
        let new = cagara_fmt::format(old).expect("parses").text;
        assert_ne!(
            old, new,
            "the sample has to change for this to test anything"
        );
        let edits = between(old, &new);
        assert_eq!(apply(old, &edits), new);
        assert!(
            edits.len() <= 3,
            "a re-laid-out file is a handful of edits, not {}",
            edits.len()
        );
    }
}
