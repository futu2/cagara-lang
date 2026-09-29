use crate::lexer::Token;
use num_derive::{FromPrimitive, ToPrimitive};

/// Token and node kinds of the lossless rowan tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, FromPrimitive, ToPrimitive)]
#[repr(u16)]
pub enum SyntaxKind {
    // ── tokens ───────────────────────────────────────────────
    Whitespace = 0,
    Comment,
    ImportKw,
    AsKw,
    SqlKw,
    FatArrow,
    Arrow,
    ComposeRight,
    AndAnd,
    OrOr,
    EqEq,
    NotEq,
    Diamond,
    LtEq,
    GtEq,
    Lt,
    Gt,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    AmpEq,
    AmpQuestion,
    AmpStar,
    AmpDot,
    AmpMinus,
    QuestionQuestion,
    Question,
    LtQuestion,
    QuestionGt,
    LtQuestionGt,
    Dollar,
    Bar,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Eq,
    Colon,
    Field,
    LeftField,
    RightField,
    Ident,
    Int,
    Float,
    String,
    Error,

    // ── nodes ────────────────────────────────────────────────
    SourceFile,
    ImportDecl,
    Definition,
    TypeAnn,
    /// `name arg arg` in a type (`query r`, `expr r bool`, `int`)
    TyApp,
    /// `{ a = int, b = string | r }`
    TyRecord,
    TyField,
    /// `a -> b`
    TyFun,
    TyParen,
    /// `x => body`
    Lambda,
    /// `lhs op rhs`
    BinExpr,
    /// `f a b`
    App,
    /// `-e`
    NegExpr,
    ParenExpr,
    RecordExpr,
    RecordField,
    ListExpr,
    /// identifier reference
    NameRef,
    /// int / float / string literal
    Literal,
    /// `.x`, `.<x`, `.>x`
    FieldExpr,
    /// `m.x` or `r.x` with no space between (qualified name or projection)
    ProjExpr,
    /// `sql "template"`
    SqlExpr,
    /// wraps unparseable input
    ErrorNode,
}

impl From<u16> for SyntaxKind {
    fn from(raw: u16) -> Self {
        num_traits::FromPrimitive::from_u16(raw).unwrap_or(SyntaxKind::Error)
    }
}

impl From<SyntaxKind> for u16 {
    fn from(kind: SyntaxKind) -> Self {
        num_traits::ToPrimitive::to_u16(&kind).unwrap()
    }
}

impl From<Token> for SyntaxKind {
    fn from(tok: Token) -> Self {
        use SyntaxKind as K;
        match tok {
            Token::Whitespace => K::Whitespace,
            Token::Comment => K::Comment,
            Token::Import => K::ImportKw,
            Token::As => K::AsKw,
            Token::Sql => K::SqlKw,
            Token::FatArrow => K::FatArrow,
            Token::Arrow => K::Arrow,
            Token::ComposeRight => K::ComposeRight,
            Token::AndAnd => K::AndAnd,
            Token::OrOr => K::OrOr,
            Token::EqEq => K::EqEq,
            Token::NotEq => K::NotEq,
            Token::Diamond => K::Diamond,
            Token::LtEq => K::LtEq,
            Token::GtEq => K::GtEq,
            Token::Lt => K::Lt,
            Token::Gt => K::Gt,
            Token::Plus => K::Plus,
            Token::Minus => K::Minus,
            Token::Star => K::Star,
            Token::Slash => K::Slash,
            Token::Percent => K::Percent,
            Token::Amp => K::Amp,
            Token::AmpEq => K::AmpEq,
            Token::AmpQuestion => K::AmpQuestion,
            Token::AmpStar => K::AmpStar,
            Token::AmpDot => K::AmpDot,
            Token::AmpMinus => K::AmpMinus,
            Token::QuestionQuestion => K::QuestionQuestion,
            Token::Question => K::Question,
            Token::LtQuestion => K::LtQuestion,
            Token::QuestionGt => K::QuestionGt,
            Token::LtQuestionGt => K::LtQuestionGt,
            Token::Dollar => K::Dollar,
            Token::Bar => K::Bar,
            Token::LParen => K::LParen,
            Token::RParen => K::RParen,
            Token::LBrace => K::LBrace,
            Token::RBrace => K::RBrace,
            Token::LBracket => K::LBracket,
            Token::RBracket => K::RBracket,
            Token::Comma => K::Comma,
            Token::Eq => K::Eq,
            Token::Colon => K::Colon,
            Token::Field => K::Field,
            Token::LeftField => K::LeftField,
            Token::RightField => K::RightField,
            Token::Ident => K::Ident,
            Token::Int => K::Int,
            Token::Float => K::Float,
            Token::String => K::String,
            Token::Error => K::Error,
        }
    }
}

impl SyntaxKind {
    /// Binary operator tokens with (left bp, right bp) and the operator
    /// function name they desugar to (`a + b` = `_+_ a b`). Operators are
    /// ordinary prelude definitions, so the core knows only their spelling.
    pub fn infix(self) -> Option<(u8, u8, &'static str)> {
        use SyntaxKind as K;
        Some(match self {
            // Left-assoc pipe and its stage shorthands (`q &? p` = `q & where p`).
            K::Amp => (1, 2, "_&_"),
            K::AmpEq => (1, 2, "_&=_"),
            K::AmpQuestion => (1, 2, "_&?_"),
            K::AmpStar => (1, 2, "_&*_"),
            K::AmpDot => (1, 2, "_&._"),
            K::AmpMinus => (1, 2, "_&-_"),
            // Joins: looser than every expression operator, tighter than the
            // pipe, so `users & teachers ? .<a == .>b` needs no parentheses.
            K::Question => (3, 4, "_?_"),
            K::LtQuestion => (3, 4, "_<?_"),
            K::QuestionGt => (3, 4, "_?>_"),
            K::LtQuestionGt => (3, 4, "_<?>_"),
            K::Dollar => (6, 5, "_$_"),       // right-assoc apply
            K::ComposeRight => (7, 8, "_>>>_"),
            K::OrOr => (9, 10, "_||_"),
            K::AndAnd => (11, 12, "_&&_"),
            K::EqEq => (13, 14, "_==_"),
            K::NotEq => (13, 14, "_!=_"),
            K::Lt => (13, 14, "_<_"),
            K::LtEq => (13, 14, "_<=_"),
            K::Gt => (13, 14, "_>_"),
            K::GtEq => (13, 14, "_>=_"),
            K::Plus => (15, 16, "_+_"),
            K::Diamond => (16, 15, "_<>_"),   // right-assoc, like Haskell's infixr 6
            K::Minus => (15, 16, "_-_"),
            K::Star => (17, 18, "_*_"),
            K::Slash => (17, 18, "_/_"),
            K::Percent => (17, 18, "_%_"),
            // Right-assoc, tightest: `.a ?? .b ?? 0`, `.score ?? 0 + 1`.
            K::QuestionQuestion => (20, 19, "_??_"),
            _ => return None,
        })
    }
}
