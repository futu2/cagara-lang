//! Lossless syntax tree and parser for Cagara.
//!
//! Every byte a user types is either lexed into a token or kept as trivia, so
//! a file always has a tree to work on — including one that failed to parse.
//! That makes this crate's input untrusted in the ordinary sense: it is
//! whatever is on disk or in an editor buffer, and it must not be able to
//! panic. The lints below are how the crate keeps that promise: an `unwrap`
//! or a slice index here is a bug report from the future, not a shortcut.
//!
//! Each remaining `#[allow]` is paired with the invariant that licenses it;
//! where the invariant was a guess rather than a proof, the code was
//! rewritten instead.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod ast;
pub mod docs;
pub mod lexer;
pub mod ops;
pub mod parser;
pub mod syntax_kind;

pub use ops::{op_name, ops, Fixity, Ops};
pub use parser::{parse, parse_with, Parse, ParseError};
pub use syntax_kind::SyntaxKind;

/// Strip a leading UTF-8 byte-order mark.
///
/// A BOM is transport metadata, not source text: editors on Windows add one,
/// and every diagnostic should point at the code rather than three bytes before
/// it. The lexer does not accept it, so callers that receive arbitrary file
/// contents (the workspace, the formatter) strip it once at the boundary.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

pub type SyntaxNode = rowan::SyntaxNode<CagaraLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<CagaraLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<CagaraLanguage>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CagaraLanguage {}

impl rowan::Language for CagaraLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        SyntaxKind::from(raw.0)
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.into())
    }
}
