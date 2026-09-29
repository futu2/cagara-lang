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
    #[token("&")]
    Amp,
    #[token("$")]
    Dollar,
    #[token("|")]
    Bar,

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

    /// Plain identifiers and operator names such as `_+_` or `_&_`.
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")]
    #[regex(r"_[+\-*/%<>=!&|$]+_")]
    Ident,

    #[regex(r"[0-9]+")]
    Int,
    #[regex(r"[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?")]
    Float,
    #[regex(r#""([^"\\]|\\.)*""#)]
    String,

    Error,
}

impl Token {
    pub fn is_trivia(self) -> bool {
        matches!(self, Token::Whitespace | Token::Comment)
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
        let line_start = !kind.is_trivia()
            && (span.start == 0 || input.as_bytes()[span.start - 1] == b'\n');
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
        lex(s).into_iter().filter(|l| !l.kind.is_trivia()).map(|l| l.kind).collect()
    }

    #[test]
    fn operators_longest_match() {
        assert_eq!(
            kinds("& && >= > >>> => -> == ="),
            vec![
                Token::Amp,
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
            vec![Token::Field, Token::LeftField, Token::RightField, Token::Ident, Token::Ident]
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
}
