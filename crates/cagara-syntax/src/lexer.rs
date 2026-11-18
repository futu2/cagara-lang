use logos::Logos;

// The `Token` enum is generated, together with its `SyntaxKind` counterpart,
// from the one table in [`crate::syntax_kind`], so the two cannot drift apart.
// It is re-exported here because this is the lexer's own vocabulary.
pub use crate::syntax_kind::Token;

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
    // The scan is driven by an explicit byte cursor rather than by one `logos`
    // lexer walked to exhaustion. Two tokens are scanned by hand — a string
    // literal and a `/* … */` block comment — because neither can be written as
    // a regular expression (the first escapes its own delimiter, the second
    // nests). A hand-scanned token is not something `logos` knows about: it
    // still offers the characters inside it as further tokens, and a greedy
    // trivia pattern (`#…` to end of line, `[ \t\r\n]+`) starting inside one can
    // run *past* its end and swallow real source. So after a hand scan the
    // cursor jumps to the token's end and a fresh lexer is built over the
    // remainder, which keeps every `logos` match inside real, unclaimed text.
    //
    // A fresh `Lexer` is a slice and a cursor, so rebuilding one per token costs
    // no more than the token walk itself.
    let mut pos = 0usize;
    while pos < input.len() {
        let rest = &input[pos..];
        // `{-` would otherwise lex as `{` then `Minus`. It cannot be a record:
        // what may follow `{` is a field name, `.name`, `}` or a type, and none
        // of them starts with `-`.
        if rest.starts_with("{-") {
            let (kind, end) = scan_block_comment(input, pos);
            out.push(Lexeme {
                kind,
                text: &input[pos..end],
                offset: pos,
                line_start: starts_line(input, pos),
            });
            pos = end;
            continue;
        }
        // A `"` is claimed by no pattern, so a string literal is scanned here
        // too. See [`scan_string`] for why it is not a `logos` pattern.
        if rest.starts_with('"') {
            let (kind, end) = scan_string(input, pos);
            out.push(Lexeme {
                kind,
                text: &input[pos..end],
                offset: pos,
                line_start: starts_line(input, pos),
            });
            pos = end;
            continue;
        }
        let mut lexer = Token::lexer(rest);
        let Some(tok) = lexer.next() else { break };
        let span = lexer.span();
        // `logos` guarantees progress, but a zero-width token would spin the
        // loop forever, so it is refused rather than trusted.
        if span.end == 0 {
            break;
        }
        let kind = tok.unwrap_or(Token::Error);
        // The whitespace regex accepts a lone `\r`, so a line break is `\n`,
        // `\r\n`, or a bare `\r`. Testing only for `\n` gave a CR-only file no
        // line starts after the first, which silently merged its definitions
        // under the parser's column-0 layout rule.
        let start = pos + span.start;
        let line_start = !kind.is_trivia() && starts_line(input, start);
        out.push(Lexeme {
            kind,
            text: &input[start..pos + span.end],
            offset: start,
            line_start,
        });
        pos += span.end;
    }
    out
}

/// Whether the byte at `offset` begins a line.
///
/// An offset past the end counts as not starting a line, so a caller does not
/// have to pre-check it.
fn starts_line(input: &str, offset: usize) -> bool {
    offset == 0
        || input
            .as_bytes()
            .get(offset.wrapping_sub(1))
            .is_none_or(|b| matches!(b, b'\n' | b'\r'))
}

/// Scan one string literal, returning its kind and the offset just past it.
///
/// The rule is the one the token's old pattern encoded, applied iteratively: a
/// literal opens with `"`, ends at the next unescaped `"`, and a `\` escapes the
/// character after it — including a `"`, which is why the scan cannot simply
/// look for the next quote. A raw `\n` or `\r` ends the line and therefore the
/// literal, which is what keeps an unclosed quote from swallowing the rest of
/// the file.
///
/// Bytes are matched rather than characters, which is safe here because every
/// byte tested is ASCII and no UTF-8 continuation byte can equal one.
fn scan_string(input: &str, start: usize) -> (Token, usize) {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'\n' | b'\r' => break,
            b'\\' => {
                // A trailing backslash, or one immediately before a line
                // break, escapes nothing in this literal. Keep the backslash
                // in the token and leave the line break for the trivia lexer.
                if bytes
                    .get(i + 1)
                    .is_none_or(|&next| matches!(next, b'\n' | b'\r'))
                {
                    return (Token::UnterminatedString, i + 1);
                }
                i += 2;
            }
            b'"' => return (Token::String, i + 1),
            _ => i += 1,
        }
    }
    (Token::UnterminatedString, i)
}

/// Scan one `/* … */` block comment, returning its kind and the offset just past
/// it.
///
/// The comment **nests**: `/* a /* b */ c */` ends at the second `*/`, not the
/// first. That is what makes a block comment able to enclose another one, which
/// is the whole point of having a block comment — commenting out a region that
/// already contains one.
///
/// An unclosed comment runs to the end of the input and is reported as
/// [`Token::UnterminatedComment`] rather than swallowing the rest of the file
/// silently. Unlike a string literal, a block comment may span lines: that is
/// the reason to use one.
fn scan_block_comment(input: &str, start: usize) -> (Token, usize) {
    let bytes = input.as_bytes();
    let mut i = start + 2;
    let mut depth = 1usize;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'{' if bytes.get(i + 1) == Some(&b'-') => {
                depth += 1;
                i += 2;
            }
            b'-' if bytes.get(i + 1) == Some(&b'}') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return (Token::BlockComment, i);
                }
            }
            _ => i += 1,
        }
    }
    (Token::UnterminatedComment, bytes.len())
}

#[cfg(test)]
mod tests {
    // Test code may assert with `unwrap`, `expect`, and indexing: a panic
    // here is a failing test, not a broken compiler.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    fn kinds(s: &str) -> Vec<Token> {
        lex(s)
            .into_iter()
            .filter(|l| !l.kind.is_trivia())
            .map(|l| l.kind)
            .collect()
    }

    /// A string literal of any length is lexed without recursing.
    ///
    /// The token used to be a `logos` pattern with an escape alternation under a
    /// star, which the derive compiles into code that recurses once per
    /// character: a few hundred kilobytes of literal overflowed the stack and
    /// aborted the process, so a file could kill the compiler (and the language
    /// server) just by being long. The scan is iterative now, and this pins that
    /// a megabyte is routine rather than fatal.
    #[test]
    fn a_long_string_literal_is_lexed_without_recursing() {
        let src = format!("\"{}\"", "x".repeat(1_000_000));
        let toks = lex(&src);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, Token::String);
        assert_eq!(toks[0].text.len(), src.len());
    }

    /// Escapes still decide where a literal ends, at any length.
    #[test]
    fn an_escaped_quote_does_not_end_a_literal() {
        let toks = lex(r#""a\"b" "c""#);
        let kinds: Vec<_> = toks
            .iter()
            .filter(|l| !l.kind.is_trivia())
            .map(|l| (l.kind, l.text))
            .collect();
        assert_eq!(
            kinds,
            vec![(Token::String, r#""a\"b""#), (Token::String, r#""c""#)]
        );
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
        let src = "x = 1 -- c\ny = 2";
        let toks = lex(src);
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, src);
        assert!(toks.iter().any(|l| l.kind == Token::Comment));
    }

    #[test]
    fn block_comments_are_trivia_and_lossless() {
        let src = "x = 1\n{- a {- nested -} still -}\ny = 2";
        let toks = lex(src);
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, src, "the token stream must rebuild the source");
        let blocks: Vec<_> = toks
            .iter()
            .filter(|l| l.kind == Token::BlockComment)
            .map(|l| l.text)
            .collect();
        assert_eq!(blocks, vec!["{- a {- nested -} still -}"]);
        // The nested `*/` does not end it, so `y` is still a token.
        assert!(toks.iter().any(|l| l.kind == Token::Ident && l.text == "y"));
    }

    #[test]
    fn a_block_comment_may_span_lines() {
        let toks = lex("{- one\n   two -}\nx");
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, "{- one\n   two -}\nx");
        assert_eq!(toks[0].kind, Token::BlockComment);
        assert_eq!(toks[0].text, "{- one\n   two -}");
    }

    /// An unclosed block comment is reported, not silently swallowed.
    ///
    /// It is trivia, so the parser never sees it; without its own kind it would
    /// run to the end of the file and every definition after the `/*` would
    /// vanish with no diagnostic.
    #[test]
    fn an_unclosed_block_comment_has_its_own_kind() {
        let toks = lex("{- never closed\nx = 1");
        assert_eq!(toks[0].kind, Token::UnterminatedComment);
        assert_eq!(toks[0].text, "{- never closed\nx = 1");
        // Still lossless.
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, "{- never closed\nx = 1");
    }

    /// A `#` *inside* a string is string content, and the `#` comment pattern
    /// must not be matched from there.
    ///
    /// The lexer used to be driven by one `logos` walk, so a greedy `#…` trivia
    /// token starting inside a hand-scanned string ran past its closing quote and
    /// swallowed the real source after it — the formatter's fuzz test caught the
    /// result as a lost token. Rebuilding the lexer at each hand-scanned
    /// boundary is what keeps `logos` inside unclaimed text.
    #[test]
    fn a_hash_inside_a_string_does_not_start_a_comment() {
        let src = "a = \"x -- not a comment\"\nb = 2";
        let toks = lex(src);
        assert!(
            !toks.iter().any(|l| l.kind == Token::Comment),
            "the `#` is inside the literal: {:?}",
            toks.iter().map(|l| (l.kind, l.text)).collect::<Vec<_>>()
        );
        let rebuilt: String = toks.iter().map(|l| l.text).collect();
        assert_eq!(rebuilt, src);
        assert!(toks.iter().any(|l| l.kind == Token::Ident && l.text == "b"));
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

    /// A comment ends at a bare `\r`, which is a line break here.
    ///
    /// The comment pattern used to stop only at `\n`, so in a CR-only file a
    /// comment ran through the line ending and swallowed the whole next line.
    /// The loss was silent: the definitions after it were inside the comment
    /// token, so nothing was reported and the tree still round-tripped.
    #[test]
    fn a_comment_ends_at_a_bare_carriage_return() {
        for src in ["a = 1 -- c\rb = 2\r", "a = 1 -- c\r\nb = 2\r\n"] {
            let idents: Vec<_> = lex(src)
                .into_iter()
                .filter(|l| l.kind == Token::Ident)
                .map(|l| l.text)
                .collect();
            assert_eq!(idents, vec!["a", "b"], "{src:?}");
            // The comment keeps its own text, and only its own.
            let comment = lex(src)
                .into_iter()
                .find(|l| l.kind == Token::Comment)
                .expect("a comment");
            assert_eq!(comment.text, "-- c", "{src:?}");
        }
    }

    #[test]
    fn unterminated_strings_stop_at_all_line_breaks_and_keep_backslashes() {
        for src in [
            "\"abc\ry = 1\r",
            "\"abc\ny = 1\n",
            "\"abc\\\ry = 1\r",
            "\"abc\\\ny = 1\n",
        ] {
            let toks = lex(src);
            assert_eq!(toks[0].kind, Token::UnterminatedString, "{src:?}");
            assert_eq!(
                toks[0].text,
                &src[..src.find(['\r', '\n']).unwrap()],
                "{src:?}"
            );
            assert_eq!(
                toks.iter().filter(|l| l.kind == Token::Ident).count(),
                1,
                "{src:?}"
            );
            let rebuilt: String = toks.iter().map(|l| l.text).collect();
            assert_eq!(rebuilt, src, "{src:?}");
        }
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
