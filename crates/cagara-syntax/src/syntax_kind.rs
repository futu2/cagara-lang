//! The kind table: every token and every node kind of the lossless tree.
//!
//! Each kind is declared once, and everything else about it is derived from
//! that one row: the lexer's [`Token`] enum (with its `logos` attributes), the
//! rowan [`SyntaxKind`], the conversion between the two, the `u16` round trip
//! and the predicates over them. Because a name exists in one place, the two
//! enums cannot drift apart and a pair cannot be mis-wired — the bug the
//! former hand-written `match` could hide, since a transposed arm still
//! compiled.
//!
//! [`Token`] is defined here rather than in [`crate::lexer`], which re-exports
//! it, because one declaration has to produce both enums.
//!
//! A row is
//!
//! ```text
//! /// doc comment (appears on both enums)
//! Name FLAGS #[logos attributes],
//! ```
//!
//! where the `FLAGS` part and the attributes are both optional, flags are
//! [`flag`] bits, and node rows have neither.

use logos::Logos;

/// Properties of a kind that consumers ask about by name, so a row reads
/// `TRIVIA` rather than carrying a bare bit mask.
pub(crate) mod flag {
    /// Skipped by the parser: whitespace and comments.
    pub(crate) const TRIVIA: u16 = 1 << 0;
    /// Can be an infix operator's spelling. Fixity and meaning are not here —
    /// they come from the operator table in [`crate::ops`], which the prelude
    /// declares — so this only says what the lexer *could* have produced.
    pub(crate) const OP: u16 = 1 << 1;
}

/// One row's flags as a bit mask. A row that carries no flags (`Error`,
/// `Ident`, every node) has none, and the empty arm says so; a separate macro
/// keeps the flag list one repetition deep, which a nested repetition inside
/// the table macro's own expansion would not be.
macro_rules! kind_flags {
    () => {
        0u16
    };
    ($($flag:ident)+) => {
        0u16 $(| flag::$flag)+
    };
}

macro_rules! kind_table {
    (
        tokens {
            $(
                $(#[doc = $doc:literal])*
                $token:ident
                $($flag:ident)*
                $(#[$logos:meta])*
                ,
            )*
        }
        nodes {
            $(
                $(#[doc = $node_doc:literal])*
                $node:ident,
            )*
        }
    ) => {
        /// Raw tokens, in the order the lexer can produce them. Whitespace and
        /// comments are kept as trivia so the rowan tree stays lossless.
        #[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Token {
            $(
                $(#[doc = $doc])*
                $(#[$logos])*
                $token,
            )*
        }

        /// Token and node kinds of the lossless rowan tree. Spellings are not
        /// here: they come from the operator table in [`crate::ops`], which
        /// the prelude declares, so this only says what the lexer *could* have
        /// produced.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u16)]
        pub enum SyntaxKind {
            // ── tokens ───────────────────────────────────────────────
            $(
                $(#[doc = $doc])*
                $token,
            )*
            // ── nodes ────────────────────────────────────────────────
            $(
                $(#[doc = $node_doc])*
                $node,
            )*
        }

        /// Every kind, in discriminant order. A kind's index here is its
        /// discriminant, which is what both `From` impls below index.
        const ALL: &[SyntaxKind] = &[$(SyntaxKind::$token,)* $(SyntaxKind::$node,)*];

        /// One [`flag`] mask per kind, in the same order. Only tokens carry
        /// flags, so this is shorter than [`ALL`] and indexed with the same
        /// discriminant.
        const FLAGS: &[u16] = &[$(kind_flags!($($flag)*)),*];

        impl SyntaxKind {
            /// The [`flag`] bits of a kind; `0` for a node.
            fn flags(self) -> u16 {
                FLAGS.get(self as usize).copied().unwrap_or(0)
            }

            /// A token that can be the spelling in an operator declaration.
            /// Every operator the lexer can produce answers `true`; nothing
            /// else does, so a declaration cannot quietly make an identifier
            /// infix.
            pub fn is_op_symbol(self) -> bool {
                self.flags() & flag::OP != 0
            }
        }

        impl Token {
            pub fn is_trivia(self) -> bool {
                SyntaxKind::from(self).flags() & flag::TRIVIA != 0
            }

            /// See [`SyntaxKind::is_op_symbol`].
            pub fn is_op_symbol(self) -> bool {
                SyntaxKind::from(self).is_op_symbol()
            }
        }

        impl From<Token> for SyntaxKind {
            /// The two enums are generated from one list, so a token's kind is
            /// its position among the tokens.
            fn from(tok: Token) -> Self {
                ALL[tok as usize]
            }
        }

        impl From<u16> for SyntaxKind {
            /// A raw kind a caller supplied that is not one of ours reads as
            /// [`SyntaxKind::Error`] rather than panicking.
            fn from(raw: u16) -> Self {
                ALL.get(raw as usize).copied().unwrap_or(SyntaxKind::Error)
            }
        }

        impl From<SyntaxKind> for u16 {
            fn from(kind: SyntaxKind) -> Self {
                kind as u16
            }
        }
    };
}

kind_table! {
    tokens {
        /// Whitespace and comments are kept as trivia so the rowan tree stays
        /// lossless.
        Whitespace TRIVIA #[regex(r"[ \t\r\n]+")],
        /// A `#` comment, running to the end of the line.
        Comment TRIVIA #[regex(r"#[^\n]*")],

        ImportKw #[token("import")],
        AsKw #[token("as")],
        SqlKw #[token("sql")],
        PrimitiveKw #[token("primitive")],

        FatArrow #[token("=>")],
        Arrow #[token("->")],
        ComposeRight OP #[token(">>>")],
        AndAnd OP #[token("&&")],
        OrOr OP #[token("||")],
        EqEq OP #[token("==")],
        NotEq OP #[token("!=")],
        Diamond OP #[token("<>")],
        LtEq OP #[token("<=")],
        GtEq OP #[token(">=")],
        Lt OP #[token("<")],
        Gt OP #[token(">")],
        Plus OP #[token("+")],
        Minus OP #[token("-")],
        Star OP #[token("*")],
        Slash OP #[token("/")],
        Percent OP #[token("%")],

        /// A `&`-shorthand: `&` followed by any operator punctuation — `&`,
        /// `&?`, `&=`, `&+`, `&*`, `&.`, `&-`, and anything of the same shape
        /// a declaration introduces (`&^`, `&>>`). Which of them exist, and
        /// what each one means, is decided by `prelude.cagara` (see
        /// [`crate::ops`]), so a new stage shorthand needs no change here.
        AmpOp OP #[regex(r"&[=!<>*/%|?$^~+\-.]*", priority = 1)],
        /// Any other operator spelling (`~=`, `|>`, `>>`), so a declared
        /// operator does not have to be a token kind. Greedy, but `-`, `+` and
        /// `.` are deliberately outside the class: `x=-1`, `a*-1` and
        /// `{a=.b}` have to keep lexing as a sign, a product and a field, and
        /// maximal munch would otherwise swallow them into one spelling nobody
        /// declared.
        Op OP #[regex(r"[=!<>*/%|?$^~]+", priority = 1)],

        QuestionQuestion OP #[token("??")],
        /// `right ? on` — `inner right on`
        Question OP #[token("?")],
        /// `right <? on` — `leftJoin right on`
        LtQuestion OP #[token("<?")],
        /// `right ?> on` — `rightJoin right on`
        QuestionGt OP #[token("?>")],
        /// `right <?> on` — `fullJoin right on`
        LtQuestionGt OP #[token("<?>")],
        Dollar OP #[token("$")],
        Bar #[token("|")],

        LParen #[token("(")],
        RParen #[token(")")],
        LBrace #[token("{")],
        RBrace #[token("}")],
        LBracket #[token("[")],
        RBracket #[token("]")],
        Comma #[token(",")],
        Eq #[token("=")],
        Colon #[token(":")],

        /// `.name` — column of the single input row
        Field #[regex(r"\.[a-zA-Z_][a-zA-Z0-9_]*")],
        /// `.<name` — column of the left join input
        LeftField #[regex(r"\.<[a-zA-Z_][a-zA-Z0-9_]*")],
        /// `.>name` — column of the right join input
        RightField #[regex(r"\.>[a-zA-Z_][a-zA-Z0-9_]*")],

        /// Plain identifiers and operator names such as `_+_` or `_&^_`. An
        /// operator name is `_`, a run of punctuation, `_` — deliberately
        /// broader than the operator classes above, because a name only has to
        /// be *definable*: whether a spelling is an operator at all is decided
        /// by those classes and by the prelude's declarations.
        Ident #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")] #[regex(r"_[^a-zA-Z0-9_\s]+_")],

        Int #[regex(r"[0-9]+")],
        Float #[regex(r"[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?")],
        /// A string ends on its line: a missing `"` must not swallow the rest
        /// of the file.
        String #[regex(r#""([^"\\\n]|\\[^\n])*""#)],
        /// `"...` with no closing quote before the end of the line. Distinct
        /// from [`SyntaxKind::Error`] so a consumer can tell a real,
        /// diagnosable token from an unlexable character.
        UnterminatedString #[regex(r#""([^"\\\n]|\\[^\n])*"#)],

        /// An unlexable character.
        Error,
    }

    nodes {
        SourceFile,
        ImportDecl,
        /// `infixl 1 &+` / `infixr 21 ??` — gives an operator its precedence
        /// and associativity. Only the prelude's declarations are honoured.
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
        /// A string literal in type position: `keyMap (prefix "u_") r`. The
        /// affix of a key mapper, read where the text still is.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw numbers rowan stores in a tree are a format: an editor may have
    /// saved one, and `kind_from_raw` reads them back. Reordering a row would
    /// silently renumber every later kind, so the whole sequence — which *is*
    /// the numbering — is pinned here. A move shows up as a text diff.
    #[test]
    fn kind_order_and_numbering_are_frozen() {
        let expected = "\
            Whitespace Comment ImportKw AsKw SqlKw PrimitiveKw FatArrow Arrow ComposeRight AndAnd \
            OrOr EqEq NotEq Diamond LtEq GtEq Lt Gt Plus Minus Star Slash Percent AmpOp Op \
            QuestionQuestion Question LtQuestion QuestionGt LtQuestionGt Dollar Bar LParen RParen \
            LBrace RBrace LBracket RBracket Comma Eq Colon Field LeftField RightField Ident Int \
            Float String UnterminatedString Error SourceFile ImportDecl OpDecl Definition TypeAnn \
            TyApp TyRecord TyField TyFun TyParen TyStr Lambda BinExpr App NegExpr ParenExpr \
            RecordExpr RecordField ListExpr NameRef Literal FieldExpr ProjExpr SqlExpr PrimitiveExpr \
            ErrorNode";
        let got: Vec<String> = ALL.iter().map(|k| format!("{k:?}")).collect();
        assert_eq!(got.join(" "), expected);
    }
}
