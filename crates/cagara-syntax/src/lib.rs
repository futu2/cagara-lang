pub mod ast;
pub mod lexer;
pub mod ops;
pub mod parser;
pub mod syntax_kind;

pub use ops::{op_name, ops, Fixity, Ops};
pub use parser::{parse, parse_with, Parse, ParseError};
pub use syntax_kind::SyntaxKind;

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
