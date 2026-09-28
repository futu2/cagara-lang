pub mod ast;
pub mod lexer;
pub mod parser;
pub mod syntax_kind;

pub use parser::{parse, Parse, ParseError};
pub use rowan::GreenNode;
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
