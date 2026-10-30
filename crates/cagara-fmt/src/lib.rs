//! Source formatter for Cagara.
//!
//! Works on the lossless rowan tree: every significant token is printed from
//! the tree and comments are recovered from the trivia around it, so nothing
//! is dropped. Layout follows three rules the grammar depends on:
//!
//! - Only the first token of an item may start a line in column 0; every
//!   other line break is indented.
//! - `m.x` (projection) stays tight and `f .x` (application) keeps its space.
//! - Items that fail to parse are printed exactly as written.
//!
//! As a final guard the output is re-parsed; if its tree differs from the
//! input's (ignoring whitespace and comment placement) formatting fails
//! rather than change the program.
//!
//! The formatter runs on whatever is in the buffer, including a half-typed
//! file with no valid parse, so like the parser it must not panic: a bail is
//! handled, a panic loses the user's formatting (and, in an editor, looks
//! like the formatter silently doing nothing).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod doc;

use cagara_syntax::{
    ops, parse, Fixity, ParseError, SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken,
};
use doc::{broken_group, concat, fits_flat, group, indent, text, Doc};
use rowan::{NodeOrToken, WalkEvent};
use std::cell::Cell;
use std::fmt;

/// Target line width.
pub const WIDTH: usize = 100;

/// The formatted source, plus the syntax errors of the input. Items with
/// syntax errors are left as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatted {
    pub text: String,
    pub errors: Vec<ParseError>,
}

/// The formatter produced output that parses differently from the input.
/// This is a formatter bug; the input should be left unchanged.
///
/// It carries the output it would have produced and where the two trees first
/// differ, so a report of a tripped guard is reproducible rather than a bare
/// "internal error".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmtError {
    /// The output that would have replaced the input.
    pub text: String,
    /// Where the input's and the output's trees first differ.
    pub difference: String,
}

impl fmt::Display for FmtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "internal formatter error: the output would change the program ({})",
            self.difference
        )
    }
}

impl std::error::Error for FmtError {}

pub fn format(src: &str) -> Result<Formatted, FmtError> {
    let parsed = parse(src);
    let root = parsed.syntax();
    let f = Fmt {
        errors: &parsed.errors,
        skip_leading: Cell::new(None),
    };
    let text = doc::print(&f.file(&root), WIDTH);
    verify_shape(&root, &text)?;
    Ok(Formatted {
        text,
        errors: parsed.errors,
    })
}

/// The formatter's last line of defence: the output must parse to the same
/// shape as the input. Separated from [`format`] so the failure path can be
/// tested with a deliberately wrong text.
fn verify_shape(input: &SyntaxNode, text: &str) -> Result<(), FmtError> {
    let (before, before_comments) = skeleton(input);
    let reparsed = parse(text);
    let (after, after_comments) = skeleton(&reparsed.syntax());
    if before == after && before_comments == after_comments {
        return Ok(());
    }
    // The pair comes out of the zip, so no index has to be trusted against
    // either slice's length.
    let at = before
        .iter()
        .zip(&after)
        .enumerate()
        .find(|(_, (a, b))| a != b);
    let difference = match at {
        Some((i, (a, b))) => format!("first difference at token {i}: `{a}` became `{b}`"),
        None => format!(
            "token count changed from {} to {}",
            before.len(),
            after.len()
        ),
    };
    Err(FmtError {
        text: text.to_string(),
        difference,
    })
}

/// `name : ty` laid out like a signature within `width`: flat if it fits,
/// else records one field per line and `->` chains one arrow per line.
/// `ty` is printed type syntax (as the checker shows it); anything that
/// does not parse as a type is returned on one line, unchanged.
pub fn format_type(name: &str, ty: &str, width: usize) -> String {
    let flat = format!("{name} : {ty}");
    // Parse under a placeholder name: operators (`_+_`) are not identifiers.
    let src = format!("x : {ty} = x\n");
    let parsed = parse(&src);
    if !parsed.errors.is_empty() {
        return flat;
    }
    let f = Fmt {
        errors: &parsed.errors,
        skip_leading: Cell::new(None),
    };
    let root = parsed.syntax();
    // The source has exactly one definition, whose type annotation is the one
    // to lay out. Matching the `TypeAnn` node by kind rather than by position
    // keeps this working if the definition's children are ever reordered.
    let ann = root
        .descendants()
        .find(|n| n.kind() == K::TypeAnn)
        .and_then(|a| a.children().next());
    let Some(Ok(doc)) = ann.map(|t| f.ty(&t)) else {
        return flat;
    };
    let out = doc::print(&concat(vec![text(format!("{name} : ")), doc]), width);
    let out = out.trim_end().to_string();
    // Only whitespace may change.
    let squash = |s: &str| s.split_whitespace().collect::<String>();
    if squash(&out) == squash(&flat) {
        out
    } else {
        flat
    }
}

/// 1-based line and column (in chars) of a byte offset.
pub fn line_col(src: &str, offset: usize) -> (usize, usize) {
    // An offset inside a multi-byte character counts as that character's
    // start: `src.get` would otherwise fail to slice and fall back to the
    // whole source, reporting the last line instead.
    let mut offset = offset.min(src.len());
    while offset > 0 && !src.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &src[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (
        before.matches('\n').count() + 1,
        before[line_start..].chars().count() + 1,
    )
}

/// Tree shape and significant tokens, plus comment texts in order.
fn skeleton(root: &SyntaxNode) -> (Vec<String>, Vec<String>) {
    let (mut shape, mut comments) = (Vec::new(), Vec::new());
    for ev in root.preorder_with_tokens() {
        match ev {
            WalkEvent::Enter(NodeOrToken::Node(n)) => shape.push(format!("{:?}(", n.kind())),
            WalkEvent::Leave(NodeOrToken::Node(_)) => shape.push(")".into()),
            WalkEvent::Enter(NodeOrToken::Token(t)) => match t.kind() {
                K::Whitespace => {}
                K::Comment => comments.push(t.text().trim_end().to_string()),
                // An unlexable run or an unterminated string takes in the
                // whitespace after it, which is the formatter's to change.
                K::Error | K::UnterminatedString => {
                    shape.push(format!("{:?} {}", t.kind(), t.text().trim_end()))
                }
                k => shape.push(format!("{k:?} {}", t.text())),
            },
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
    (shape, comments)
}

/// The node is not in the shape this formatter expects; print it verbatim.
struct Bail;

type R = Result<Doc, Bail>;

fn is_trivia(k: K) -> bool {
    matches!(k, K::Whitespace | K::Comment)
}

/// Children without trivia.
fn sig(n: &SyntaxNode) -> Vec<SyntaxElement> {
    n.children_with_tokens()
        .filter(|e| !is_trivia(e.kind()))
        .collect()
}

fn sig_tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> {
    n.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !is_trivia(t.kind()))
}

fn token(e: Option<&SyntaxElement>, k: K) -> Result<SyntaxToken, Bail> {
    match e.and_then(|e| e.as_token()) {
        Some(t) if t.kind() == k => Ok(t.clone()),
        _ => Err(Bail),
    }
}

/// A record or row-type field name, which may be a reserved word.
///
/// `{case = .x}` and `{sql = string}` are legal, and the parser keeps the
/// word's own token kind, so the name is the first token that can be one
/// rather than necessarily an `Ident`.
fn field_name_tok(e: &SyntaxElement) -> Result<SyntaxToken, Bail> {
    match e.as_token() {
        Some(t)
            if matches!(
                t.kind(),
                K::Ident | K::SqlKw | K::CaseKw | K::PrimitiveKw | K::ImportKw | K::AsKw | K::LetKw
            ) =>
        {
            Ok(t.clone())
        }
        _ => Err(Bail),
    }
}

fn node(e: Option<&SyntaxElement>) -> Result<SyntaxNode, Bail> {
    e.and_then(|e| e.as_node()).cloned().ok_or(Bail)
}

/// Trivia tokens directly before `t`, in source order.
fn trivia_before(t: &SyntaxToken) -> Vec<SyntaxToken> {
    let mut out: Vec<_> = std::iter::successors(t.prev_token(), |p| p.prev_token())
        .take_while(|p| is_trivia(p.kind()))
        .collect();
    out.reverse();
    out
}

fn has_comment_before(t: &SyntaxToken) -> bool {
    trivia_before(t).iter().any(|p| p.kind() == K::Comment)
}

/// Any comment strictly inside `n` (trivia never ends a well-formed node).
fn has_inner_comment(n: &SyntaxNode) -> bool {
    n.descendants_with_tokens().any(|e| e.kind() == K::Comment)
}

/// Does `field`'s value bring its own lines, so that its record should break
/// one field per line around it?
///
/// True for a `case` holding a comment, and for one written over several
/// lines in the source: the language reference writes such a `case` starting
/// on the line after `case {`, and a record in that shape keeps the shape
/// rather than inlining the first arm into `{ a = 1, b = case { ...;` on one
/// line. Only records ask; a `case` in an argument list is laid out like any
/// other argument.
fn multiline_value(field: &SyntaxNode) -> bool {
    let els = sig(field);
    let [_, _, value] = els.as_slice() else {
        return false;
    };
    match value.as_node() {
        Some(v) if v.kind() == K::CaseExpr => has_inner_comment(v) || spelled_over_lines(v),
        _ => false,
    }
}

/// Whether the whitespace between the braces of a `case` holds a line break.
/// Only trivia separates the first brace from the last, so the slice between
/// them is exactly the text between them.
fn spelled_over_lines(n: &SyntaxNode) -> bool {
    let mut braces = n
        .children_with_tokens()
        .filter(|e| matches!(e.kind(), K::LBrace | K::RBrace));
    let (Some(open), Some(close)) = (braces.next(), braces.last()) else {
        return false;
    };
    let base = n.text_range().start();
    let start = open.text_range().end() - base;
    let end = close.text_range().start() - base;
    n.text().slice(start..end).to_string().contains('\n')
}

fn comment_text(t: &SyntaxToken) -> Doc {
    text(t.text().trim_end())
}

/// Binding powers of a `BinExpr`'s operator, from the language's fixed
/// operator table.
fn bin_parts(n: &SyntaxNode) -> Result<(SyntaxNode, SyntaxToken, SyntaxNode, (u8, u8)), Bail> {
    let els = sig(n);
    let [l, op, r] = els.as_slice() else {
        return Err(Bail);
    };
    let op = op.as_token().ok_or(Bail)?.clone();
    let bp = ops().find(op.text()).map(Fixity::precedence).ok_or(Bail)?;
    Ok((node(Some(l))?, op, node(Some(r))?, bp))
}

/// Whether `n` is a pipeline expression: a chain of stage operators (§4.2),
/// which is what breaks one stage per line.
fn is_pipeline(n: &SyntaxNode) -> bool {
    n.kind() == K::BinExpr && bin_parts(n).is_ok_and(|(_, op, _, _)| is_pipe(&op))
}

/// Does this operator build a pipeline stage? That is a property of the
/// operator table — an operator is a stage when it sits at `&`'s level — so the
/// formatter has no list of its own to keep in step.
fn is_pipe(op: &SyntaxToken) -> bool {
    ops().is_stage(op.text())
}

struct Fmt<'a> {
    errors: &'a [ParseError],
    /// Start of the current item's first token, whose leading comments the
    /// file loop has already printed.
    skip_leading: Cell<Option<usize>>,
}

impl Fmt<'_> {
    // ── comments ─────────────────────────────────────────────

    /// Own-line comments in a trivia run, each on a fresh line and keeping
    /// one blank line before it if the source had one. A comment on the same
    /// line as the previous token is its trailing comment and skipped here.
    /// Also returns whether a blank line ends the run.
    fn comment_run(&self, trivia: &[SyntaxToken], has_prev: bool) -> (Vec<Doc>, bool) {
        let (mut out, mut newlines, mut first) = (Vec::new(), 0, true);
        for t in trivia {
            if t.kind() == K::Whitespace {
                newlines += t.text().matches('\n').count();
                continue;
            }
            if !(has_prev && first && newlines == 0) {
                out.push(if newlines >= 2 {
                    Doc::BlankLine
                } else {
                    Doc::HardLine
                });
                out.push(comment_text(t));
            }
            first = false;
            newlines = 0;
        }
        (out, newlines >= 2)
    }

    /// Own-line comments before `t`. At the top level a blank line before
    /// the item is kept even without comments.
    fn leading(&self, t: &SyntaxToken, top: bool) -> Doc {
        let has_prev =
            std::iter::successors(t.prev_token(), |p| p.prev_token()).any(|p| !is_trivia(p.kind()));
        let (mut out, blank) = self.comment_run(&trivia_before(t), has_prev);
        if !out.is_empty() || (top && blank) {
            out.push(if blank { Doc::BlankLine } else { Doc::HardLine });
        }
        concat(out)
    }

    /// A comment after `t` on the same line.
    fn trailing(&self, t: &SyntaxToken) -> Doc {
        for n in std::iter::successors(t.next_token(), |n| n.next_token()) {
            match n.kind() {
                K::Whitespace if !n.text().contains('\n') => {}
                K::Comment => return Doc::LineSuffix(n.text().trim_end().into()),
                _ => break,
            }
        }
        concat(vec![])
    }

    /// A token with its comments.
    fn tok(&self, t: &SyntaxToken) -> Doc {
        let start = usize::from(t.text_range().start());
        let lead = if self.skip_leading.get() == Some(start) {
            concat(vec![])
        } else {
            self.leading(t, false)
        };
        concat(vec![lead, self.tok_body(t)])
    }

    /// A token and its trailing comment, without leading comments.
    fn tok_body(&self, t: &SyntaxToken) -> Doc {
        concat(vec![text(t.text()), self.trailing(t)])
    }

    // ── items ────────────────────────────────────────────────

    fn file(&self, root: &SyntaxNode) -> Doc {
        let items: Vec<(SyntaxNode, SyntaxToken)> = root
            .children()
            .filter_map(|n| sig_tokens(&n).next().map(|t| (n, t)))
            .collect();
        let starts: Vec<usize> = items
            .iter()
            .map(|(_, t)| usize::from(t.text_range().start()))
            .collect();
        let mut parts = Vec::new();
        for (i, (item, first)) in items.iter().enumerate() {
            // `starts` is derived from `items` in the same pass, so the two
            // have the same length; taking the pair from one lookup keeps
            // that from being something the loop has to remember.
            let Some(&start) = starts.get(i) else {
                continue;
            };
            parts.push(Doc::HardLine);
            parts.push(self.leading(first, true));
            self.skip_leading.set(Some(start));
            let next = starts.get(i + 1).copied().unwrap_or(usize::MAX);
            let dirty = item.kind() == K::ErrorNode
                || item
                    .descendants_with_tokens()
                    .any(|e| matches!(e.kind(), K::ErrorNode | K::Error | K::UnterminatedString))
                || self
                    .errors
                    .iter()
                    .any(|e| e.offset > start && e.offset <= next);
            let d = if dirty { Err(Bail) } else { self.item(item) };
            parts.push(d.unwrap_or_else(|Bail| self.verbatim(item)));
        }
        // Comments after the last token.
        let last = sig_tokens(root).last();
        let trivia: Vec<SyntaxToken> = match &last {
            Some(l) => std::iter::successors(l.next_token(), |n| n.next_token()).collect(),
            None => std::iter::successors(root.first_token(), |n| n.next_token()).collect(),
        };
        parts.extend(self.comment_run(&trivia, last.is_some()).0);
        concat(parts)
    }

    /// The item's source from its first to its last token.
    ///
    /// Sliced from the **item's** text, not the root's. `SyntaxText::slice`
    /// walks the green children from the node it hangs off, so slicing the root
    /// once per item made formatting a file of unparseable items quadratic:
    /// 20 000 dirty items took 44 seconds where parsing them took 5
    /// milliseconds. Both endpoints are descendants of `item`, so the same
    /// range expressed relative to it is the same text.
    fn verbatim(&self, item: &SyntaxNode) -> Doc {
        let mut toks = sig_tokens(item);
        let (Some(first), last) = (toks.next(), toks.last()) else {
            return concat(vec![]);
        };
        let last = last.unwrap_or_else(|| first.clone());
        let base = item.text_range().start();
        let (start, end) = (first.text_range().start(), last.text_range().end());
        let src = item.text().slice((start - base)..(end - base)).to_string();
        concat(vec![text(src), self.trailing(&last)])
    }

    fn item(&self, n: &SyntaxNode) -> R {
        match n.kind() {
            K::ImportDecl => self.spaced(n),
            K::Definition => self.def(n),
            _ => Err(Bail),
        }
    }

    /// A node made only of tokens, separated by single spaces.
    fn spaced(&self, n: &SyntaxNode) -> R {
        let mut parts = Vec::new();
        for (i, e) in sig(n).iter().enumerate() {
            if i > 0 {
                parts.push(text(" "));
            }
            parts.push(self.tok(e.as_token().ok_or(Bail)?));
        }
        Ok(concat(parts))
    }

    /// `name [: type] = body`
    fn def(&self, n: &SyntaxNode) -> R {
        let els = sig(n);
        let mut it = els.iter();
        let mut parts = vec![self.tok(&token(it.next(), K::Ident)?)];
        let mut next = it.next();
        if next.is_some_and(|e| e.kind() == K::Colon) {
            let colon = token(next, K::Colon)?;
            let ann = node(it.next())?;
            let ty = ann.children().next().ok_or(Bail)?;
            // Indented so a comment inside the type can never push a name
            // into column 0.
            parts.push(text(" "));
            parts.push(indent(concat(vec![
                self.tok(&colon),
                text(" "),
                self.ty(&ty)?,
            ])));
            next = it.next();
        }
        let eq = token(next, K::Eq)?;
        let body = node(it.next())?;
        if it.next().is_some() {
            return Err(Bail);
        }
        parts.push(text(" "));
        parts.push(self.tok(&eq));
        parts.push(self.rhs(&body, true)?);
        Ok(concat(parts))
    }

    /// What follows `=` or `=>`: on the same line if the body brings its own
    /// line breaks (record, list, parens, case, lambda), otherwise moved to an
    /// indented line when it does not fit. A `let` brings its own lines too and
    /// is still moved: see [`huggable`](Fmt::huggable).
    ///
    /// A pipeline is never hugged. Hugging put the first stage on the `=` line
    /// and the stages after it at the item's indent, so the head sat in a
    /// different column from the `&`s and read as part of the definition's
    /// line rather than as the pipeline's first stage. Moved down, the stages
    /// start at one column and the name sits above them — the head is then one
    /// stage among the others, which is what it is.
    ///
    /// When the body moves, whether it is grouped decides which brace of a
    /// `}` its line break lands on, so it is chosen here rather than taken
    /// from the body: an ungrouped `Line` ends at the first direct one, which
    /// would put a record field's `case {` down on the next line while the
    /// record waits for that line to end. A body that brings its own lines
    /// breaks the group, so a long `case` is laid out over its arms and not
    /// by moving whole. A comment before the body breaks it for the same
    /// reason: a `Line` in unbroken mode is a space, which is no place for a
    /// comment.
    fn rhs(&self, body: &SyntaxNode, top: bool) -> R {
        let first = sig_tokens(body).next().ok_or(Bail)?;
        let gap = has_comment_before(&first);
        let doc = self.expr(body, top)?;
        if !gap && self.huggable(body) {
            return Ok(concat(vec![text(" "), doc]));
        }
        let d = indent(concat(vec![Doc::Line, doc]));
        Ok(if gap || d.forces_break() {
            broken_group(d)
        } else {
            group(d)
        })
    }

    fn huggable(&self, n: &SyntaxNode) -> bool {
        match n.kind() {
            // A definition body's `case` is not hugged: `x = case {` followed
            // by arms says nothing about where the arm list belongs, so a
            // `case` that does not fit moves to an indented line whole and
            // opens its arms there. One that fits stays on the `=` line like
            // any body. A record field's `case` is hugged — `b = case {` is
            // the shape the reference writes, and the field decides whether
            // the record breaks around it (`multiline_value`).
            K::CaseExpr => n.parent().is_some_and(|p| p.kind() == K::RecordField),
            K::RecordExpr | K::ListExpr | K::ParenExpr | K::Lambda => true,
            // A `let` is not hugged, though it too brings its own lines. Hugged,
            // `value => let weighted =` stayed on the arrow's line while the
            // binding's value and its `in` clause broke one level in from the
            // definition, so the three parts of the binding sat in three
            // places and the `let` read as a word on the arrow's line rather
            // than as the head of a block. Moved down whole, `let`, the value
            // and `in` stand in one column each and the block hangs off the
            // `=` / `=>` it belongs to — the shape a `let` forms anywhere else.
            K::LetExpr => false,
            K::App => self.hugs_last_arg(n),
            _ => false,
        }
    }

    /// `f a { ... }`: an application whose last argument is bracketed keeps
    /// its arguments on one line and lets the bracket break.
    ///
    /// The hug is only worth taking when the bracket is what breaks and what
    /// follows it has room. A hug prints the arguments flat around the
    /// bracket, so a call whose hugged parts are too wide prints `ifThenElse (`
    /// on one line, the condition indented under it, and
    /// `) (toYuan .lstbal) (toYuan .balance)` stranded after the `)`. That
    /// only beats the width by moving text to the left of a broken bracket,
    /// which is where a reader stops counting arguments. Such a call uses the
    /// ordinary argument layout, which packs them. A comment on the argument's
    /// own line (after its bracket opens, before it closes) is left to the
    /// bracket, which breaks; one deeper inside belongs to an element of it.
    fn hugs_last_arg(&self, n: &SyntaxNode) -> bool {
        let Some(last) = n.children().last() else {
            return false;
        };
        if !matches!(last.kind(), K::RecordExpr | K::ListExpr | K::ParenExpr)
            || last.children_with_tokens().any(|e| e.kind() == K::Comment)
        {
            return false;
        }
        // Only a paren can be squeezed like this: `( … )` has no line-break
        // points of its own beyond the contents, so hugging it is how a
        // predicate argument gets the width. Records and lists always break
        // at their `,`s and read fine that way, and a record holding a field
        // that must break (a long value) would fail a flat measurement for a
        // reason that has nothing to do with the hug.
        if last.kind() != K::ParenExpr {
            return true;
        }
        let Ok(args) = self.args(n) else {
            return false;
        };
        // The whole call, head first: the head is part of what has to fit for
        // the hug to be worth taking.
        let flat = concat(
            args.iter()
                .enumerate()
                .flat_map(|(i, a)| {
                    if i == 0 {
                        vec![a.clone()]
                    } else {
                        vec![text(" "), a.clone()]
                    }
                })
                .collect::<Vec<_>>(),
        );
        fits_flat(&flat, WIDTH)
    }

    /// An application's arguments, printed, head first.
    fn args(&self, n: &SyntaxNode) -> Result<Vec<Doc>, Bail> {
        sig(n)
            .iter()
            .map(|e| self.expr(&node(Some(e))?, false))
            .collect()
    }

    // ── expressions ──────────────────────────────────────────

    /// `top` is set for a definition body (and the bodies of lambdas at the
    /// top of one), where a pipeline of two or more stages always breaks.
    fn expr(&self, n: &SyntaxNode, top: bool) -> R {
        match n.kind() {
            K::NameRef | K::Literal | K::FieldExpr | K::SqlExpr => self.spaced(n),
            K::ProjExpr => {
                let els = sig(n);
                let [base, field] = els.as_slice() else {
                    return Err(Bail);
                };
                Ok(concat(vec![
                    self.expr(&node(Some(base))?, false)?,
                    self.tok(&token(Some(field), K::Field)?),
                ]))
            }
            K::NegExpr => {
                let els = sig(n);
                let [minus, e] = els.as_slice() else {
                    return Err(Bail);
                };
                Ok(concat(vec![
                    self.tok(&token(Some(minus), K::Minus)?),
                    self.expr(&node(Some(e))?, false)?,
                ]))
            }
            K::ParenExpr => self.paren(n, |inner| self.expr(inner, false)),
            K::RecordExpr => self.delimited(n, K::LBrace, K::RBrace, Doc::Line),
            K::ListExpr => self.delimited(n, K::LBracket, K::RBracket, Doc::SoftLine),
            K::App => self.app(n),
            K::BinExpr => self.bin(n, top),
            K::Lambda => self.lambda(n, top),
            K::CaseExpr => self.case(n),
            K::LetExpr => self.let_expr(n, top),
            _ => Err(Bail),
        }
    }

    /// `x => body`, or `(x : ty) => body`.
    ///
    /// The parameter and its annotation stay together before the arrow: an
    /// annotation is part of what the arrow binds, not something hung off it,
    /// and `(x : string) =>` reads like the signature it mirrors. Only the body
    /// goes through [`rhs`](Fmt::rhs), so a long lambda still breaks after the
    /// arrow exactly as a bare one does.
    fn lambda(&self, n: &SyntaxNode, top: bool) -> R {
        let els = sig(n);
        let Some(arrow) = els.iter().position(|e| e.kind() == K::FatArrow) else {
            return Err(Bail);
        };
        let (Some(head), Some(tail)) = (els.get(..arrow), els.get(arrow..)) else {
            return Err(Bail);
        };
        let [body] = tail.get(1..).ok_or(Bail)? else {
            return Err(Bail);
        };
        let body = node(Some(body))?;
        let arrow = self.tok(&token(tail.first(), K::FatArrow)?);
        // A pipeline body moves down after the arrow when it does not fit, the
        // way any other long body does. Hugging it would leave `=> head` on
        // the `=` line however wide it grew: the stages break after `&`, but
        // nothing measures the first line, so it could run past the width.
        // The stages then sit flush with the head (see [`bin`](Fmt::bin)).
        if is_pipeline(&body) {
            let gap = has_comment_before(&sig_tokens(&body).next().ok_or(Bail)?);
            let d = indent(concat(vec![Doc::Line, self.expr(&body, top)?]));
            let moved = if gap || d.forces_break() {
                broken_group(d)
            } else {
                group(d)
            };
            return Ok(concat(vec![
                self.lambda_param(head)?,
                text(" "),
                arrow,
                moved,
            ]));
        }
        Ok(concat(vec![
            self.lambda_param(head)?,
            text(" "),
            arrow,
            self.rhs(&body, top)?,
        ]))
    }

    /// A lambda's parameter before the arrow: `name`, or `(name : ty)`.
    fn lambda_param(&self, els: &[SyntaxElement]) -> R {
        match els {
            [param] => Ok(self.tok(&token(Some(param), K::Ident)?)),
            [open, name, colon, ann, close] => Ok(concat(vec![
                self.tok(&token(Some(open), K::LParen)?),
                self.tok(&token(Some(name), K::Ident)?),
                text(" "),
                self.tok(&token(Some(colon), K::Colon)?),
                text(" "),
                self.ty(&node(Some(ann))?.children().next().ok_or(Bail)?)?,
                self.tok(&token(Some(close), K::RParen)?),
            ])),
            _ => Err(Bail),
        }
    }

    /// `let name = value in body`.
    ///
    /// Flat when it fits; otherwise the `in` clause moves to an indented line,
    /// so a long binding does not push the body off the width. The value is
    /// hung off the `=` and breaks on its own line only when it does not fit
    /// beside it: a binding whose value is short (`let y = 1`) keeps its three
    /// words together, while a long one opens the block — `let weighted =` on a
    /// line of its own with the value indented under it. Breaking both
    /// unconditionally put a one-token value on a line by itself just because
    /// something later in the body was long.
    ///
    /// The whole form is not hugged to a `=` / `=>`; see
    /// [`huggable`](Fmt::huggable).
    fn let_expr(&self, n: &SyntaxNode, top: bool) -> R {
        let els = sig(n);
        let [let_tok, name, eq, value, in_tok, body] = els.as_slice() else {
            return Err(Bail);
        };
        let in_tok = token(Some(in_tok), K::Ident)?;
        if in_tok.text() != "in" {
            return Err(Bail);
        }
        // `let name = value` measured at the column it starts in, so the value
        // stays on the `=` line whenever it fits there.
        let binding = concat(vec![
            self.tok(&token(Some(let_tok), K::LetKw)?),
            text(" "),
            self.tok(&token(Some(name), K::Ident)?),
            text(" "),
            self.tok(&token(Some(eq), K::Eq)?),
            indent(concat(vec![
                Doc::Line,
                self.expr(&node(Some(value))?, false)?,
            ])),
        ]);
        Ok(group(concat(vec![
            group(binding),
            indent(concat(vec![
                Doc::Line,
                self.tok(&in_tok),
                text(" "),
                self.expr(&node(Some(body))?, top)?,
            ])),
        ])))
    }

    /// `case { c1 => v1; c2 => v2 }`: flat if it fits, else one arm per line.
    ///
    /// The arms break as a unit inside the braces, the way a record's fields
    /// and a type's arrows do: `case {` closes the first line, every arm and
    /// its `;` gets a line of its own, and `}` closes the last one. Nothing is
    /// hung off the opening line, so a `case` laid out over several lines is
    /// stable: written that way in the source, it comes back that way, and a
    /// long one is still broken only at its `;`s rather than mid-arm.
    ///
    /// The node's first element is the `case` keyword, so the braces are found
    /// by kind rather than by position: `sig` is `case`, `{`, arms and `;`s,
    /// `}`. An optional trailing `;` before `}` is kept as written.
    fn case(&self, n: &SyntaxNode) -> R {
        // A `case` written over several lines keeps that layout when it is a
        // record field, which is where the reference lays it out that way.
        let keep_layout = n
            .parent()
            .is_some_and(|p| p.kind() == K::RecordField && spelled_over_lines(n));
        let els = sig(n);
        let keyword = token(els.first(), K::CaseKw)?;
        let (open_at, close_at) = match (
            els.iter().position(|e| e.kind() == K::LBrace),
            els.iter().rposition(|e| e.kind() == K::RBrace),
        ) {
            (Some(o), Some(c)) if o < c => (o, c),
            _ => return Err(Bail),
        };
        let (open, close) = (
            token(els.get(open_at), K::LBrace)?,
            token(els.get(close_at), K::RBrace)?,
        );
        // `open_at < close_at` is in scope, so the range is in bounds and
        // cannot be reversed; `unwrap_or_default` states that rather than
        // relying on the reader to re-derive it.
        let middle = els.get(open_at + 1..close_at).unwrap_or_default();
        let head = concat(vec![self.tok(&keyword), text(" "), self.tok(&open)]);
        // `case {}` has no arm to break after; the checker rejects it, but the
        // formatter must still lay it out rather than bail, so a file being
        // typed keeps formatting.
        if middle.is_empty() && !has_inner_comment(n) {
            return Ok(concat(vec![head, self.tok(&close)]));
        }
        // Every arm, and its `;` when it has one, is one entry. The trailing
        // `;` belongs to the last arm, so it never opens a line of its own.
        let mut lines: Vec<Vec<Doc>> = Vec::new();
        for e in middle {
            match e {
                NodeOrToken::Node(a) if a.kind() == K::CaseArm => lines.push(vec![self.arm(a)?]),
                NodeOrToken::Token(t) if t.kind() == K::Semicolon => match lines.last_mut() {
                    Some(line) => line.push(self.tok(t)),
                    // `case {; ... }`: nothing to attach it to, so it opens
                    // its own line rather than being moved.
                    None => lines.push(vec![self.tok(t)]),
                },
                _ => return Err(Bail),
            }
        }
        let mut inner = vec![Doc::Line];
        for (i, line) in lines.into_iter().enumerate() {
            if i > 0 {
                inner.push(Doc::Line);
            }
            inner.extend(line);
        }
        inner.push(self.leading(&close, false));
        // A comment breaks the `case` (it would not fit on one line), and an
        // arm laid out over several lines breaks it the way such a field
        // breaks a record. A `case` written over several lines that does fit
        // on one was folded, unless it is a record field keeping its layout.
        let broken = has_inner_comment(n) || inner.iter().any(Doc::forces_break) || keep_layout;
        // The pad is one space, the way a record's is; broken, it becomes the
        // line break that puts `}` at the arms' level.
        let d = concat(vec![
            head,
            indent(concat(inner)),
            Doc::LineThenSpace,
            self.tok_body(&close),
        ]);
        Ok(if broken { broken_group(d) } else { group(d) })
    }

    /// `cond => value`, one item of a `case` arm line.
    ///
    /// The condition is not printed with `rhs`, which could move it to its own
    /// line: `=>` is a fixed separator, not a place a `case` may break, so the
    /// arm stays together and only its own operands break. A value that brings
    /// its own lines (a record, a nested `case`) indents those lines from the
    /// arm, so an inner `}` cannot land at the arm's own level, where it would
    /// read as the outer `}`.
    fn arm(&self, n: &SyntaxNode) -> R {
        let els = sig(n);
        let [cond, arrow, value] = els.as_slice() else {
            return Err(Bail);
        };
        let value = node(Some(value))?;
        Ok(concat(vec![
            self.expr(&node(Some(cond))?, false)?,
            text(" "),
            self.tok(&token(Some(arrow), K::FatArrow)?),
            indent(concat(vec![text(" "), self.expr(&value, false)?])),
        ]))
    }

    fn paren(&self, n: &SyntaxNode, inner: impl Fn(&SyntaxNode) -> R) -> R {
        let els = sig(n);
        let [open, e, close] = els.as_slice() else {
            return Err(Bail);
        };
        let close = token(Some(close), K::RParen)?;
        let d = concat(vec![
            self.tok(&token(Some(open), K::LParen)?),
            indent(concat(vec![
                Doc::SoftLine,
                inner(&node(Some(e))?)?,
                self.leading(&close, false),
            ])),
            Doc::SoftLine,
            self.tok_body(&close),
        ]);
        Ok(if has_inner_comment(n) {
            broken_group(d)
        } else {
            group(d)
        })
    }

    /// `{ a, b | r }` / `[a, b]`: flat if it fits, else one element per line.
    /// Commas are kept exactly as written (including a trailing one).
    ///
    /// An element laid out over several lines breaks the list around it, the
    /// way a comment does: kept flat, the `,`s after it would not break, so a
    /// multi-line `case` in a record would be folded into `..., b = case {` on
    /// one line.
    fn delimited(&self, n: &SyntaxNode, open: K, close: K, pad: Doc) -> R {
        let els = sig(n);
        let (Some(first), Some(last)) = (els.first(), els.last()) else {
            return Err(Bail);
        };
        let (open, close) = (token(Some(first), open)?, token(Some(last), close)?);
        // `els` is non-empty (both ends were just taken), so `len() - 1` is
        // the last index and the range is in bounds; `get` says so.
        let middle = els.get(1..els.len().saturating_sub(1)).unwrap_or_default();
        if middle.is_empty() && !has_inner_comment(n) {
            return Ok(concat(vec![self.tok(&open), self.tok(&close)]));
        }
        let mut inner = vec![pad.clone()];
        // Whether an element brings its own line breaks, held onto until the
        // element list is built: it breaks the list the way a comment does.
        let mut multiline = false;
        for (i, e) in middle.iter().enumerate() {
            match e {
                NodeOrToken::Node(c) => {
                    let d = match c.kind() {
                        // A `case` written one arm per line keeps that
                        // layout: laid out flat it would have to be broken
                        // again, and the next run would see a one-line source
                        // and fold it.
                        K::RecordField if multiline_value(c) => {
                            let v = self
                                .field(c, |v| Ok(concat(vec![text(" "), self.expr(v, false)?])))?;
                            multiline |= v.forces_break();
                            v
                        }
                        K::RecordField => self.field(c, |v| self.rhs(v, false))?,
                        K::TyField => {
                            self.field(c, |v| Ok(concat(vec![text(" "), self.ty(v)?])))?
                        }
                        _ if close.kind() == K::RBracket => self.expr(c, false)?,
                        _ => return Err(Bail),
                    };
                    multiline |= d.forces_break();
                    inner.push(d);
                }
                NodeOrToken::Token(t) => match t.kind() {
                    K::Comma => {
                        inner.push(self.tok(t));
                        if i + 1 < middle.len() {
                            inner.push(Doc::Line);
                        }
                    }
                    // `{ | r }` has no field before the bar to break after.
                    K::Bar if i == 0 => inner.extend([self.tok(t), text(" ")]),
                    K::Bar => inner.extend([Doc::Line, self.tok(t), text(" ")]),
                    K::Ident => inner.push(self.tok(t)),
                    _ => return Err(Bail),
                },
            }
        }
        inner.push(self.leading(&close, false));
        let d = concat(vec![
            self.tok(&open),
            indent(concat(inner)),
            pad,
            self.tok_body(&close),
        ]);
        Ok(if has_inner_comment(n) || multiline {
            broken_group(d)
        } else {
            group(d)
        })
    }

    /// `name = value`, or the shorthand `.name` for `name = .name`, which is
    /// not a node the value printer can be applied to.
    fn field(&self, n: &SyntaxNode, value: impl Fn(&SyntaxNode) -> R) -> R {
        let els = sig(n);
        // The shorthand: a single `.name` token in place of `name = .name`.
        if let [only] = els.as_slice() {
            return Ok(self.tok(&token(Some(only), K::Field)?));
        }
        let [name, eq, v] = els.as_slice() else {
            return Err(Bail);
        };
        // The name is reprinted from its own token, so a reserved word used as
        // a field name (`{case = .x}`) survives formatting unchanged.
        Ok(concat(vec![
            self.tok(&field_name_tok(name)?),
            text(" "),
            self.tok(&token(Some(eq), K::Eq)?),
            value(&node(Some(v))?)?,
        ]))
    }

    /// `f a b`: flat if it fits, else one argument per indented line.
    ///
    /// The whole list breaks together. Packing kept two arguments on a line
    /// and the rest below, so which line an argument was on depended on its
    /// length, and an argument that opened a bracket left the ones after it
    /// past the `)` — `) (same_day process_date)` — which is where a reader
    /// stops counting arguments. One per line puts every opening token in the
    /// same column: the call reads as a list of arguments, and an argument
    /// that opens a bracket simply gets its contents indented under it.
    fn app(&self, n: &SyntaxNode) -> R {
        let args = self.args(n)?;
        let Some((head, rest)) = args.split_first() else {
            return Err(Bail);
        };
        if self.hugs_last_arg(n) {
            let mut parts = vec![head.clone()];
            for a in rest {
                parts.extend([text(" "), a.clone()]);
            }
            return Ok(concat(parts));
        }
        let tail = rest.iter().flat_map(|a| [Doc::Line, a.clone()]).collect();
        let d = concat(vec![head.clone(), indent(concat(tail))]);
        Ok(if has_inner_comment(n) {
            broken_group(d)
        } else {
            group(d)
        })
    }

    /// A chain of operators at one precedence level is laid out as a unit:
    /// flat if it fits, else one operator per line, leading.
    fn bin(&self, n: &SyntaxNode, top: bool) -> R {
        let (_, op, _, bp) = bin_parts(n)?;
        let (mut operands, mut ops) = (Vec::new(), Vec::new());
        flatten(n, bp, &mut operands, &mut ops)?;
        // `flatten` always leaves at least the node's own operand, so this is
        // the whole chain; the split says what each half is.
        let Some((first, rest)) = operands.split_first() else {
            return Err(Bail);
        };
        let mut tail = Vec::new();
        for (op, e) in ops.iter().zip(rest) {
            tail.extend([Doc::Line, self.tok(op), text(" "), self.expr(e, false)?]);
        }
        // A pipeline that its parent moved to a line of its own — a
        // definition's body and a record field's value (see
        // [`rhs`](Fmt::rhs)), a lambda's body (see
        // [`lambda`](Fmt::lambda)) — has been indented there already:
        // indenting the stages again would put them under the head instead of
        // beside it, and `=` / `=>` / head / `& stage` reads as peers. A
        // pipeline that follows a token on its line (an argument, an arm of a
        // `case`, a `let`'s `in`) is not one of these: there the stages indent
        // under the head.
        let parent = n.parent();
        let in_lambda = parent.as_ref().is_some_and(|p| p.kind() == K::Lambda);
        let moved =
            in_lambda || parent.is_some_and(|p| matches!(p.kind(), K::Definition | K::RecordField));
        let stages = concat(tail);
        let d = concat(vec![
            self.expr(first, false)?,
            if moved { stages } else { indent(stages) },
        ]);
        // A definition body's pipeline of two or more stages always shows one
        // stage per line, which is the shape the reference writes. A lambda's
        // body is not that: it is one expression among the definition's, and
        // a short one belongs on the arrow's line.
        let force = has_inner_comment(n) || (top && !in_lambda && is_pipe(&op) && ops.len() >= 2);
        Ok(if force { broken_group(d) } else { group(d) })
    }

    // ── types ────────────────────────────────────────────────

    fn ty(&self, n: &SyntaxNode) -> R {
        match n.kind() {
            K::TyApp => {
                let mut parts = Vec::new();
                for (i, e) in sig(n).iter().enumerate() {
                    if i > 0 {
                        parts.push(text(" "));
                    }
                    parts.push(match e {
                        NodeOrToken::Token(t) if i == 0 && t.kind() == K::Ident => self.tok(t),
                        NodeOrToken::Node(c) if i > 0 => self.ty(c)?,
                        _ => return Err(Bail),
                    });
                }
                Ok(concat(parts))
            }
            K::TyParen => self.paren(n, |inner| self.ty(inner)),
            K::TyRecord => self.delimited(n, K::LBrace, K::RBrace, Doc::Line),
            K::TyFun => {
                // `a -> b -> c` is right-nested; print the chain as a unit.
                let (mut operands, mut arrows) = (Vec::new(), Vec::new());
                let mut cur = n.clone();
                while cur.kind() == K::TyFun {
                    let els = sig(&cur);
                    let [a, arrow, b] = els.as_slice() else {
                        return Err(Bail);
                    };
                    operands.push(node(Some(a))?);
                    arrows.push(token(Some(arrow), K::Arrow)?);
                    cur = node(Some(b))?;
                }
                operands.push(cur);
                // `operands` is the chain's left operand followed by each
                // right one, so `split_first` is the pairing `arrows` was
                // built for.
                let Some((first, rest)) = operands.split_first() else {
                    return Err(Bail);
                };
                let mut tail = Vec::new();
                for (arrow, t) in arrows.iter().zip(rest) {
                    tail.extend([Doc::Line, self.tok(arrow), text(" "), self.ty(t)?]);
                }
                let d = concat(vec![self.ty(first)?, indent(concat(tail))]);
                Ok(if has_inner_comment(n) {
                    broken_group(d)
                } else {
                    group(d)
                })
            }
            _ => Err(Bail),
        }
    }
}

/// Operands and operators of the same-precedence chain rooted at `n`,
/// following the side its associativity nests on.
fn flatten(
    n: &SyntaxNode,
    bp: (u8, u8),
    operands: &mut Vec<SyntaxNode>,
    ops: &mut Vec<SyntaxToken>,
) -> Result<(), Bail> {
    let (l, op, r, _) = bin_parts(n)?;
    let same = |c: &SyntaxNode| c.kind() == K::BinExpr && bin_parts(c).is_ok_and(|p| p.3 == bp);
    let left_assoc = bp.0 < bp.1;
    if left_assoc && same(&l) {
        flatten(&l, bp, operands, ops)?;
    } else {
        operands.push(l);
    }
    ops.push(op);
    if !left_assoc && same(&r) {
        flatten(&r, bp, operands, ops)
    } else {
        operands.push(r);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
