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
    PrimitiveKw,
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
    /// A `&`-shorthand (`&+`, `&?`, …), whose fixity the prelude declares.
    AmpOp,
    /// Any other operator spelling the prelude declares (`~=`, `|>`).
    Op,
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
    /// A string literal that ran to the end of the line without closing.
    /// Distinct from [`SyntaxKind::Error`] so a consumer can tell a real,
    /// diagnosable token from an unlexable character.
    UnterminatedString,
    Error,

    // ── nodes ────────────────────────────────────────────────
    SourceFile,
    ImportDecl,
    /// `infixl 1 &+` / `infixr 21 ??` — gives an operator its precedence and
    /// associativity. Only the prelude's declarations are honoured.
    OpDecl,
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
    /// A string literal in type position: `keyMap (prefix "u_") r`. The affix
    /// of a key mapper, read where the text still is.
    TyStr,
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
    /// `primitive "name"`
    PrimitiveExpr,
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
            Token::Primitive => K::PrimitiveKw,
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
            Token::AmpOp => K::AmpOp,
            Token::Op => K::Op,
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
            Token::UnterminatedString => K::UnterminatedString,
            Token::Error => K::Error,
        }
    }
}

impl SyntaxKind {
    /// A token that can be an infix operator's spelling. Fixity and meaning
    /// are not here: they come from the operator table in `crate::ops`, which
    /// the prelude declares, so this only says what the lexer *could* have
    /// produced.
    pub fn is_op_symbol(self) -> bool {
        use SyntaxKind as K;
        matches!(
            self,
            K::AmpOp
                | K::Op
                | K::ComposeRight
                | K::AndAnd
                | K::OrOr
                | K::EqEq
                | K::NotEq
                | K::Diamond
                | K::LtEq
                | K::GtEq
                | K::Lt
                | K::Gt
                | K::Plus
                | K::Minus
                | K::Star
                | K::Slash
                | K::Percent
                | K::QuestionQuestion
                | K::Question
                | K::LtQuestion
                | K::QuestionGt
                | K::LtQuestionGt
                | K::Dollar
        )
    }
}
