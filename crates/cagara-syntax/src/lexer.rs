use logos::Logos;

/// Raw tokens. Whitespace and comments are kept as trivia so the rowan tree
/// stays lossless.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    #[regex(r"[ \t\r\n]+")]
    Whitespace,
    #[regex(r"#[^\n]*")]
    Comment,

    #[token("import")]
    Import,
    #[token("as")]
    As,
    #[token("sql")]
    Sql,
    #[token("primitive")]
    Primitive,

    #[token("=>")]
    FatArrow,
    #[token("->")]
    Arrow,
    #[token(">>>")]
    ComposeRight,
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<>")]
    Diamond,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("??")]
    QuestionQuestion,
    /// `right ? on` — `inner right on`
    #[token("?")]
    Question,
    /// `right <? on` — `leftJoin right on`
    #[token("<?")]
    LtQuestion,
    /// `right ?> on` — `rightJoin right on`
    #[token("?>")]
    QuestionGt,
    /// `right <?> on` — `fullJoin right on`
    #[token("<?>")]
    LtQuestionGt,
    #[token("$")]
    Dollar,
    #[token("|")]
    Bar,

    /// A `&`-shorthand: `&` followed by any operator punctuation — `&`, `&?`,
    /// `&=`, `&+`, `&*`, `&.`, `&-`, and anything of the same shape a
    /// declaration introduces (`&^`, `&>>`). Which of them exist, and what
    /// each one means, is decided by `prelude.cagara` (see [`crate::ops`]), so
    /// a new stage shorthand needs no change here.
    #[regex(r"&[=!<>*/%|?$^~+\-.]*", priority = 1)]
    AmpOp,
    /// Any other operator spelling (`~=`, `|>`, `>>`), so a declared operator
    /// does not have to be a token kind. Greedy, but `-`, `+` and `.` are
    /// deliberately outside the class: `x=-1`, `a*-1` and `{a=.b}` have to
    /// keep lexing as a sign, a product and a field, and maximal munch would
    /// otherwise swallow them into one spelling nobody declared.
    #[regex(r"[=!<>*/%|?$^~]+", priority = 1)]
    Op,

    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(",")]
    Comma,
    #[token("=")]
    Eq,
    #[token(":")]
    Colon,

    /// `.name` — column of the single input row
    #[regex(r"\.[a-zA-Z_][a-zA-Z0-9_]*")]
    Field,
    /// `.<name` — column of the left join input
    #[regex(r"\.<[a-zA-Z_][a-zA-Z0-9_]*")]
    LeftField,
    /// `.>name` — column of the right join input
    #[regex(r"\.>[a-zA-Z_][a-zA-Z0-9_]*")]
    RightField,

    /// Plain identifiers and operator names such as `_+_` or `_&^_`. An
    /// operator name is `_`, a run of punctuation, `_` — deliberately broader
    /// than the operator classes above, because a name only has to be
    /// *definable*: whether a spelling is an operator at all is decided by
    /// those classes and by the prelude's declarations.
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")]
    #[regex(r"_[^a-zA-Z0-9_\s]+_")]
    Ident,

    #[regex(r"[0-9]+")]
    Int,
    #[regex(r"[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?")]
    Float,
    /// A string ends on its line: a missing `"` must not swallow the rest
    /// of the file.
    #[regex(r#""([^"\\\n]|\\[^\n])*""#)]
    String,
    /// `"...` with no closing quote before the end of the line.
    #[regex(r#""([^"\\\n]|\\[^\n])*"#)]
    UnterminatedString,

    Error,
}

impl Token {
    pub fn is_trivia(self) -> bool {
        matches!(self, Token::Whitespace | Token::Comment)
    }

    /// A token that can be the spelling in an operator declaration. Every
    /// operator the lexer can produce answers `true`; nothing else does, so a
    /// declaration cannot quietly make an identifier infix.
    pub fn is_op_symbol(self) -> bool {
        use crate::syntax_kind::SyntaxKind as K;
        K::from(self).is_op_symbol()
    }
}

/// A lexed token with its text, byte offset, and whether it is the first
/// non-trivia token on a line starting at column 0 (used for the layout rule
/// that ends top-level definitions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexeme<'a> {
    pub kind: Token,
    pub text: &'a str,
    pub offset: usize,
    pub line_start: bool,
}

pub fn lex(input: &str) -> Vec<Lexeme<'_>> {
    let mut out = Vec::new();
    let mut lexer = Token::lexer(input);
    while let Some(tok) = lexer.next() {
        let span = lexer.span();
        let kind = tok.unwrap_or(Token::Error);
        let line_start =
            !kind.is_trivia() && (span.start == 0 || input.as_bytes()[span.start - 1] == b'\n');
        out.push(Lexeme {
            kind,
            text: &input[span.clone()],
            offset: span.start,
            line_start,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<Token> {
        lex(s)
            .into_iter()
            .filter(|l| !l.kind.is_trivia())
            .map(|l| l.kind)
            .collect()
    }

    #[test]
    fn operators_longest_match() {
        assert_eq!(
            kinds("& && >= > >>> => -> == ="),
            vec![
                Token::AmpOp,
                Token::AndAnd,
                Token::GtEq,
                Token::Gt,
                Token::ComposeRight,
                Token::FatArrow,
                Token::Arrow,
                Token::EqEq,
                Token::Eq
            ]
        );
    }

    #[test]
    fn fields_and_operator_idents() {
        assert_eq!(
            kinds(".id .<user_id .>id _+_ _&_"),
            vec![
                Token::Field,
                Token::LeftField,
                Token::RightField,
                Token::Ident,
                Token::Ident
            ]
        );
    }

    #[test]
    fn comments_are_trivia_and_lossless() {
        let src = "x = 1 # c\ny = 2";
        let toks = lex(src);
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, src);
        assert!(toks.iter().any(|l| l.kind == Token::Comment));
    }

    #[test]
    fn line_start_marks_column_zero() {
        let toks: Vec<_> = lex("a = 1\n  & b\nc = 2")
            .into_iter()
            .filter(|l| l.line_start)
            .map(|l| l.text)
            .collect();
        assert_eq!(toks, vec!["a", "c"]);
    }

    #[test]
    fn stage_join_and_coalesce_operators() {
        use Token::*;
        assert_eq!(
            kinds("&= &? &* &. &- &+ ?? ? <? ?> <?> && <= <>"),
            vec![
                AmpOp,
                AmpOp,
                AmpOp,
                AmpOp,
                AmpOp,
                AmpOp,
                QuestionQuestion,
                Question,
                LtQuestion,
                QuestionGt,
                LtQuestionGt,
                AndAnd,
                LtEq,
                Diamond
            ]
        );
        // Join operators next to join-side fields, and operator names.
        assert_eq!(
            kinds("<?.<a ?>.>b"),
            vec![LtQuestion, LeftField, QuestionGt, RightField]
        );
        assert_eq!(
            kinds("_&?_ _<?>_ _??_ _&._ _&+_"),
            vec![Ident, Ident, Ident, Ident, Ident]
        );
    }

    /// A stage shorthand must not swallow the `&` of the next stage, and `&+`
    /// must not lex as `&` followed by `+`.
    #[test]
    fn amp_plus_is_one_token() {
        use Token::*;
        assert_eq!(kinds("&+"), vec![AmpOp]);
        assert_eq!(kinds("& +"), vec![AmpOp, Plus]);
        // `&+` in a pipeline, next to another shorthand.
        assert_eq!(
            kinds("q &+ {a = 1} &= {.a}"),
            vec![Ident, AmpOp, LBrace, Ident, Eq, Int, RBrace, AmpOp, LBrace, Field, RBrace]
        );
    }

    /// The shorthand family is not enumerated in the lexer: any `&` followed
    /// by operator punctuation is one token, so a newly declared `&^` needs no
    /// lexer change. `&&` stays its own operator.
    #[test]
    fn any_amp_shorthand_is_one_token() {
        use Token::*;
        for s in ["&", "&+", "&^", "&>>", "&%", "&~"] {
            assert_eq!(kinds(s), vec![AmpOp], "{s}");
        }
        assert_eq!(kinds("&&"), vec![AndAnd]);
        assert_eq!(kinds("& &"), vec![AmpOp, AmpOp]);
    }

    /// A spelling the lexer can produce must also be *definable*: the two
    /// halves of a shorthand are the declaration and the `_name_` definition,
    /// so if `&^` lexes as an operator then `_&^_` has to lex as a name.
    #[test]
    fn every_operator_spelling_has_a_definable_name() {
        for s in [
            "+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "<>", "&&", "||", "??", "?",
            "<?", "?>", "<?>", "$", ">>>", "<<<", "~=", "|>", ">>", "^", "&", "&?", "&=", "&+",
            "&*", "&.", "&-", "&^", "&>>", "&%", "&~",
        ] {
            let name = format!("_{s}_");
            let toks: Vec<_> = lex(&name)
                .into_iter()
                .filter(|l| !l.kind.is_trivia())
                .collect();
            assert_eq!(toks.len(), 1, "`{name}` is not one token: {toks:?}");
            assert_eq!(toks[0].kind, Token::Ident, "`{name}` is not a name");
            assert_eq!(toks[0].text, name);
        }
    }

    /// An operator spelling outside the `&` family lexes as one `Op`, so it
    /// too can be declared; but `-`, `+` and `.` stay out of its class, so a
    /// unary sign, a product or a field is never swallowed into one.
    #[test]
    fn other_operators_lex_but_never_swallow_a_sign_or_field() {
        use Token::*;
        assert_eq!(kinds("~="), vec![Op]);
        assert_eq!(kinds("|>"), vec![Op]);
        assert_eq!(kinds(">>"), vec![Op]);
        // Maximal munch must not eat these.
        assert_eq!(kinds("x=-1"), vec![Ident, Eq, Minus, Int]);
        assert_eq!(kinds("a*-1"), vec![Ident, Star, Minus, Int]);
        assert_eq!(kinds("{a=.b}"), vec![LBrace, Ident, Eq, Field, RBrace]);
        assert_eq!(kinds("a<>-b"), vec![Ident, Diamond, Minus, Ident]);
        // Named operators still win over the generic rule.
        assert_eq!(
            kinds("== >>> ?? <?> $ | ||"),
            vec![
                EqEq,
                ComposeRight,
                QuestionQuestion,
                LtQuestionGt,
                Dollar,
                Bar,
                OrOr
            ]
        );
    }
}
