//! A small Wadler-style document model and printer.
//!
//! A `Group` is printed flat (every `Line` a space, every `SoftLine`
//! nothing) if it fits in the remaining width, otherwise broken (every
//! direct `Line` / `SoftLine` a newline at the current indent). A group that
//! contains a `HardLine` or multi-line text is always broken.

use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Doc {
    Text(Rc<str>),
    Concat(Rc<[Doc]>),
    Indent(Rc<Doc>),
    Group {
        doc: Rc<Doc>,
        broken: bool,
    },
    /// Space when flat, newline when broken.
    Line,
    /// Nothing when flat, newline when broken.
    SoftLine,
    /// Always a newline (at most one in a row).
    HardLine,
    /// Always an empty line (at most one in a row, never at the start).
    BlankLine,
    /// Text deferred to the end of the current line (trailing comments).
    LineSuffix(Rc<str>),
}

pub fn text(s: impl Into<Rc<str>>) -> Doc {
    Doc::Text(s.into())
}

pub fn concat(parts: Vec<Doc>) -> Doc {
    Doc::Concat(parts.into())
}

pub fn indent(d: Doc) -> Doc {
    Doc::Indent(Rc::new(d))
}

pub fn group(d: Doc) -> Doc {
    let broken = d.forces_break();
    Doc::Group {
        doc: Rc::new(d),
        broken,
    }
}

/// A group printed broken even if it would fit.
pub fn broken_group(d: Doc) -> Doc {
    Doc::Group {
        doc: Rc::new(d),
        broken: true,
    }
}

impl Doc {
    /// Whether this doc can never print on one line. Groups cache the answer,
    /// so this is linear in the doc built since the enclosing group.
    fn forces_break(&self) -> bool {
        match self {
            Doc::Text(s) => s.contains('\n'),
            Doc::Concat(ds) => ds.iter().any(Doc::forces_break),
            Doc::Indent(d) => d.forces_break(),
            Doc::Group { broken, .. } => *broken,
            Doc::HardLine | Doc::BlankLine => true,
            Doc::Line | Doc::SoftLine | Doc::LineSuffix(_) => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

struct Printer {
    out: String,
    width: usize,
    /// Column of the next character, in chars.
    col: usize,
    at_line_start: bool,
    suffix: Vec<Rc<str>>,
}

pub fn print(doc: &Doc, width: usize) -> String {
    let mut p = Printer {
        out: String::new(),
        width,
        col: 0,
        at_line_start: true,
        suffix: Vec::new(),
    };
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((ind, mode, d)) = stack.pop() {
        match d {
            Doc::Text(s) => p.text(ind, s),
            Doc::Concat(ds) => stack.extend(ds.iter().rev().map(|d| (ind, mode, d))),
            Doc::Indent(d) => stack.push((ind + 2, mode, d)),
            Doc::Group { doc, broken } => {
                let flat = !*broken && (mode == Mode::Flat || p.fits(ind, doc, &stack));
                stack.push((ind, if flat { Mode::Flat } else { Mode::Break }, doc));
            }
            Doc::Line if mode == Mode::Flat => p.text(ind, " "),
            Doc::SoftLine if mode == Mode::Flat => {}
            Doc::Line | Doc::SoftLine | Doc::HardLine => p.newline(),
            Doc::BlankLine => p.blank_line(),
            Doc::LineSuffix(s) => p.suffix.push(s.clone()),
        }
    }
    p.newline();
    p.out
}

impl Printer {
    fn text(&mut self, ind: usize, s: &str) {
        // A separator space at the start of a line (after a comment forced a
        // newline) would only become indentation noise.
        if s.is_empty() || (self.at_line_start && s.trim().is_empty()) {
            return;
        }
        // A trailing comment ends its line: whatever follows it goes on the
        // next one, indented (column 0 would start a new item), instead of
        // being printed before the comment.
        let ind = if !self.suffix.is_empty() && !s.trim().is_empty() {
            self.newline();
            ind.max(2)
        } else {
            ind
        };
        if self.at_line_start {
            self.out.extend(std::iter::repeat_n(' ', ind));
            self.col = ind;
            self.at_line_start = false;
        }
        self.out.push_str(s);
        match s.rfind('\n') {
            Some(i) => self.col = s[i + 1..].chars().count(),
            None => self.col += s.chars().count(),
        }
        // Verbatim text can end its own line.
        if s.ends_with('\n') {
            let trimmed = self.out.trim_end_matches([' ', '\t', '\r', '\n']).len();
            self.out.truncate(trimmed);
            self.out.push('\n');
            self.at_line_start = true;
        }
    }

    /// End the current line, if anything is on it.
    fn newline(&mut self) {
        if !self.suffix.is_empty() {
            let trimmed = self.out.trim_end_matches(' ').len();
            self.out.truncate(trimmed);
        }
        for s in std::mem::take(&mut self.suffix) {
            self.out.push(' ');
            self.out.push_str(&s);
            self.at_line_start = false;
        }
        if self.at_line_start {
            return;
        }
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        self.out.push('\n');
        self.at_line_start = true;
        self.col = 0;
    }

    fn blank_line(&mut self) {
        self.newline();
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    /// Does `doc`, flat, plus whatever follows it up to the next possible
    /// newline, fit in the rest of the line?
    fn fits(&self, ind: usize, doc: &Doc, rest: &[(usize, Mode, &Doc)]) -> bool {
        let mut left =
            self.width as isize - if self.at_line_start { ind } else { self.col } as isize;
        let mut todo: Vec<(Mode, &Doc)> = vec![(Mode::Flat, doc)];
        let mut rest = rest.iter().rev();
        loop {
            let Some((mode, d)) = todo.pop().or_else(|| rest.next().map(|&(_, m, d)| (m, d)))
            else {
                return true;
            };
            match d {
                Doc::Text(s) => {
                    if s.contains('\n') {
                        return false;
                    }
                    left -= s.chars().count() as isize;
                }
                Doc::Concat(ds) => todo.extend(ds.iter().rev().map(|d| (mode, d))),
                Doc::Indent(d) => todo.push((mode, d)),
                Doc::Group { doc, broken } => {
                    todo.push((if *broken { Mode::Break } else { mode }, doc))
                }
                Doc::Line if mode == Mode::Flat => left -= 1,
                Doc::SoftLine if mode == Mode::Flat => {}
                Doc::Line | Doc::SoftLine | Doc::HardLine | Doc::BlankLine => return left >= 0,
                Doc::LineSuffix(_) => {}
            }
            if left < 0 {
                return false;
            }
        }
    }
}
