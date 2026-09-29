use crate::lexer::{lex, Lexeme, Token};
use crate::syntax_kind::SyntaxKind as K;
use crate::{CagaraLanguage, SyntaxNode};
use rowan::{Checkpoint, GreenNode, GreenNodeBuilder, Language};

/// A syntax error with the byte offset where it was detected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

/// Result of parsing: a lossless green tree plus recovered errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parse {
    pub green: GreenNode,
    pub errors: Vec<ParseError>,
}

impl Parse {
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }
}

pub fn parse(input: &str) -> Parse {
    let mut p = Parser {
        toks: lex(input),
        pos: 0,
        b: GreenNodeBuilder::new(),
        errors: Vec::new(),
        end: input.len(),
    };
    p.file();
    Parse {
        green: p.b.finish(),
        errors: p.errors,
    }
}

struct Parser<'a> {
    toks: Vec<Lexeme<'a>>,
    pos: usize,
    b: GreenNodeBuilder<'static>,
    errors: Vec<ParseError>,
    end: usize,
}

impl<'a> Parser<'a> {
    // ── token cursor ─────────────────────────────────────────

    /// Push leading trivia into the tree so it stays lossless.
    fn eat_trivia(&mut self) {
        while let Some(l) = self.toks.get(self.pos).copied() {
            if !l.kind.is_trivia() {
                break;
            }
            self.b.token(CagaraLanguage::kind_to_raw(l.kind.into()), l.text);
            self.pos += 1;
        }
    }

    fn peek_lex(&self, n: usize) -> Option<Lexeme<'a>> {
        self.toks[self.pos..].iter().filter(|l| !l.kind.is_trivia()).nth(n).copied()
    }

    fn peek(&self) -> Option<Token> {
        self.peek_lex(0).map(|l| l.kind)
    }

    fn at(&self, t: Token) -> bool {
        self.peek() == Some(t)
    }

    /// Layout rule: an identifier or `import` in column 0 starts a new item.
    fn at_boundary(&self) -> bool {
        match self.peek_lex(0) {
            None => true,
            Some(l) => l.line_start && matches!(l.kind, Token::Ident | Token::Import),
        }
    }

    fn bump(&mut self) {
        self.eat_trivia();
        if let Some(l) = self.toks.get(self.pos).copied() {
            self.b.token(CagaraLanguage::kind_to_raw(l.kind.into()), l.text);
            self.pos += 1;
        }
    }

    fn start(&mut self, k: K) {
        self.eat_trivia();
        self.b.start_node(CagaraLanguage::kind_to_raw(k));
    }

    fn finish(&mut self) {
        self.b.finish_node();
    }

    fn checkpoint(&mut self) -> Checkpoint {
        self.eat_trivia();
        self.b.checkpoint()
    }

    fn wrap(&mut self, cp: Checkpoint, k: K) {
        self.b.start_node_at(cp, CagaraLanguage::kind_to_raw(k));
        self.b.finish_node();
    }

    fn error(&mut self, message: impl Into<String>) {
        let offset = self.peek_lex(0).map(|l| l.offset).unwrap_or(self.end);
        self.errors.push(ParseError {
            offset,
            message: message.into(),
        });
    }

    fn expect(&mut self, t: Token, what: &str) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            self.error(format!("expected {what}"));
            false
        }
    }

    /// Consume tokens up to the next item boundary into an error node.
    /// Always consumes at least one token so callers cannot loop.
    fn recover(&mut self, message: &str) {
        self.error(message);
        self.start(K::ErrorNode);
        self.bump();
        while !self.at_boundary() {
            self.bump();
        }
        self.finish();
    }

    // ── items ────────────────────────────────────────────────

    fn file(&mut self) {
        self.b.start_node(CagaraLanguage::kind_to_raw(K::SourceFile));
        loop {
            match self.peek() {
                None => break,
                Some(Token::Import) => self.import(),
                Some(Token::Ident) => self.def(),
                Some(_) => self.recover("expected a definition or import"),
            }
        }
        self.eat_trivia();
        self.finish();
    }

    fn import(&mut self) {
        self.start(K::ImportDecl);
        self.bump();
        self.expect(Token::String, "import path string");
        if self.at(Token::As) {
            self.bump();
            self.expect(Token::Ident, "module alias");
        }
        self.finish();
    }

    fn def(&mut self) {
        self.start(K::Definition);
        self.bump(); // name
        if self.at(Token::Colon) {
            self.bump();
            self.start(K::TypeAnn);
            self.ty();
            self.finish();
        }
        if self.expect(Token::Eq, "`=`") {
            self.expr();
        }
        if !self.at_boundary() {
            self.recover("unexpected tokens after definition");
        }
        self.finish();
    }

    // ── types ────────────────────────────────────────────────

    fn ty(&mut self) {
        let cp = self.checkpoint();
        self.ty_app();
        if self.at(Token::Arrow) {
            self.bump();
            self.ty();
            self.wrap(cp, K::TyFun);
        }
    }

    fn ty_atom_start(&self) -> bool {
        !self.at_boundary()
            && matches!(self.peek(), Some(Token::Ident | Token::LBrace | Token::LParen))
    }

    /// `head arg arg`; a bare record or paren type is also accepted.
    fn ty_app(&mut self) {
        if self.at(Token::Ident) {
            self.start(K::TyApp);
            self.bump();
            while self.ty_atom_start() {
                self.ty_atom();
            }
            self.finish();
        } else {
            self.ty_atom();
        }
    }

    fn ty_atom(&mut self) {
        match self.peek() {
            Some(Token::Ident) => {
                self.start(K::TyApp);
                self.bump();
                self.finish();
            }
            Some(Token::LParen) => {
                self.start(K::TyParen);
                self.bump();
                self.ty();
                self.expect(Token::RParen, "`)`");
                self.finish();
            }
            Some(Token::LBrace) => {
                self.start(K::TyRecord);
                self.bump();
                while self.at(Token::Ident) {
                    self.start(K::TyField);
                    self.bump();
                    self.expect(Token::Eq, "`=` in record type");
                    self.ty();
                    self.finish();
                    if self.at(Token::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                if self.at(Token::Bar) {
                    self.bump();
                    self.expect(Token::Ident, "row variable after `|`");
                }
                self.expect(Token::RBrace, "`}`");
                self.finish();
            }
            _ => {
                self.error("expected a type");
                self.start(K::ErrorNode);
                if !self.at_boundary() {
                    self.bump();
                }
                self.finish();
            }
        }
    }

    // ── expressions ──────────────────────────────────────────

    fn at_lambda(&self) -> bool {
        self.peek() == Some(Token::Ident)
            && self.peek_lex(1).map(|l| l.kind) == Some(Token::FatArrow)
    }

    fn expr(&mut self) {
        if self.at_lambda() {
            self.start(K::Lambda);
            self.bump(); // param
            self.bump(); // =>
            self.expr();
            self.finish();
        } else {
            self.bin(0);
        }
    }

    /// Pratt loop over the operator table in `SyntaxKind::infix`.
    fn bin(&mut self, min_bp: u8) {
        let cp = self.checkpoint();
        self.unary();
        loop {
            if self.at_boundary() {
                break;
            }
            let Some(tok) = self.peek() else { break };
            let Some((l_bp, r_bp, _)) = K::from(tok).infix() else { break };
            if l_bp < min_bp {
                break;
            }
            self.bump();
            if self.at_lambda() {
                // allow `q & x => ...` style right operands
                self.expr();
            } else {
                self.bin(r_bp);
            }
            self.wrap(cp, K::BinExpr);
        }
    }

    fn unary(&mut self) {
        if self.at(Token::Minus) {
            self.start(K::NegExpr);
            self.bump();
            self.unary();
            self.finish();
        } else {
            self.app();
        }
    }

    fn atom_start(&self) -> bool {
        !self.at_boundary()
            && !self.at_lambda()
            && matches!(
                self.peek(),
                Some(
                    Token::Ident
                        | Token::Int
                        | Token::Float
                        | Token::String
                        | Token::Field
                        | Token::LeftField
                        | Token::RightField
                        | Token::LParen
                        | Token::LBrace
                        | Token::LBracket
                        | Token::Sql
                )
            )
    }

    /// `f a b` — juxtaposition; a single atom is not wrapped.
    fn app(&mut self) {
        let cp = self.checkpoint();
        self.atom();
        let mut applied = false;
        while self.atom_start() {
            self.atom();
            applied = true;
        }
        if applied {
            self.wrap(cp, K::App);
        }
    }

    /// Is the raw token right after the current one a `.field` with no
    /// whitespace in between? (`m.x` is projection, `f .x` is application.)
    fn adjacent_field(&self) -> bool {
        let mut i = self.pos;
        while self.toks.get(i).is_some_and(|l| l.kind.is_trivia()) {
            i += 1;
        }
        self.toks.get(i + 1).map(|l| l.kind) == Some(Token::Field)
    }

    fn atom(&mut self) {
        if self.at_boundary() {
            self.error("expected an expression");
            self.start(K::ErrorNode);
            self.finish();
            return;
        }
        match self.peek() {
            Some(Token::Ident) => {
                let cp = self.checkpoint();
                let proj = self.adjacent_field();
                self.start(K::NameRef);
                self.bump();
                self.finish();
                if proj {
                    // chain: a.b.c
                    while self.toks.get(self.pos).map(|l| l.kind) == Some(Token::Field) {
                        self.bump();
                        self.wrap(cp, K::ProjExpr);
                    }
                }
            }
            Some(Token::Int | Token::Float | Token::String) => {
                self.start(K::Literal);
                self.bump();
                self.finish();
            }
            Some(Token::Field | Token::LeftField | Token::RightField) => {
                self.start(K::FieldExpr);
                self.bump();
                self.finish();
            }
            Some(Token::Sql) => {
                self.start(K::SqlExpr);
                self.bump();
                self.expect(Token::String, "SQL template string after `sql`");
                self.finish();
            }
            Some(Token::LParen) => {
                self.start(K::ParenExpr);
                self.bump();
                self.expr();
                self.expect(Token::RParen, "`)`");
                self.finish();
            }
            Some(Token::LBrace) => self.record(),
            Some(Token::LBracket) => self.list(),
            _ => {
                self.error("expected an expression");
                self.start(K::ErrorNode);
                self.bump();
                self.finish();
            }
        }
    }

    fn record(&mut self) {
        self.start(K::RecordExpr);
        self.bump(); // {
        while self.at(Token::Ident) {
            self.start(K::RecordField);
            self.bump();
            self.expect(Token::Eq, "`=` in record");
            self.expr();
            self.finish();
            if self.at(Token::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(Token::RBrace, "`}`");
        self.finish();
    }

    fn list(&mut self) {
        self.start(K::ListExpr);
        self.bump(); // [
        while !self.at(Token::RBracket) && !self.at_boundary() {
            self.expr();
            if self.at(Token::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(Token::RBracket, "`]`");
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact s-expression of node kinds (tokens omitted except names).
    fn sexp(n: &SyntaxNode) -> String {
        let mut s = format!("({:?}", n.kind());
        for c in n.children_with_tokens() {
            match c {
                rowan::NodeOrToken::Node(n) => {
                    s.push(' ');
                    s.push_str(&sexp(&n));
                }
                rowan::NodeOrToken::Token(t) => {
                    if matches!(t.kind(), K::Ident | K::Int | K::Field) && n.kind() != K::Definition {
                        s.push(' ');
                        s.push_str(t.text());
                    }
                }
            }
        }
        s.push(')');
        s
    }

    fn body(src: &str) -> String {
        let p = parse(src);
        assert!(p.errors.is_empty(), "errors: {:?}", p.errors);
        let def = p.syntax().children().next().unwrap();
        let e = def.children().filter(|c| c.kind() != K::TypeAnn).last().unwrap();
        sexp(&e)
    }

    #[test]
    fn precedence_pipe_app_plus() {
        assert_eq!(
            body("x = a & f b + 1"),
            "(BinExpr (NameRef a) (BinExpr (App (NameRef f) (NameRef b)) (Literal 1)))"
        );
    }

    #[test]
    fn curried_lambda() {
        assert_eq!(
            body("f = x => y => x + y"),
            "(Lambda x (Lambda y (BinExpr (NameRef x) (NameRef y))))"
        );
    }

    #[test]
    fn projection_vs_field_argument() {
        assert_eq!(body("x = m.users"), "(ProjExpr (NameRef m) .users)");
        assert_eq!(
            body("x = upper .name"),
            "(App (NameRef upper) (FieldExpr .name))"
        );
    }

    #[test]
    fn layout_ends_definitions() {
        let src = "a = users\n  & where (.age >= 18)\nb = 2\n";
        let p = parse(src);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let defs: Vec<_> = p.syntax().children().collect();
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn types_parse() {
        let p = parse("u : query { id = int, age = int | r } -> expr r bool = x");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let ann = p.syntax().descendants().find(|n| n.kind() == K::TypeAnn).unwrap();
        assert!(ann.descendants().any(|n| n.kind() == K::TyFun));
        assert!(ann.descendants().any(|n| n.kind() == K::TyRecord));
    }

    #[test]
    fn recovery_and_lossless() {
        let src = "a = (1 +\nb = 2\n";
        let p = parse(src);
        assert!(!p.errors.is_empty());
        assert_eq!(p.syntax().children().filter(|n| n.kind() == K::Definition).count(), 2);
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn operator_definition() {
        let p = parse("_+_ : expr r int -> expr r int -> expr r int = sql \"$1 + $2\"");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(p.syntax().descendants().any(|n| n.kind() == K::SqlExpr));
    }

    #[test]
    fn join_binds_between_pipe_and_expressions() {
        // `a & (b ? (x == y))`, then the stage shorthand applies to the join.
        assert_eq!(
            body("q = a & b ? .<x == .>y &= r"),
            "(BinExpr (BinExpr (NameRef a) (BinExpr (NameRef b) (BinExpr (FieldExpr) (FieldExpr)))) (NameRef r))"
        );
        // Stage shorthands are left-assoc at the level of `&`.
        assert_eq!(
            body("q = a &? p && r &- 3"),
            "(BinExpr (BinExpr (NameRef a) (BinExpr (NameRef p) (NameRef r))) (Literal 3))"
        );
    }

    #[test]
    fn coalesce_is_right_assoc_and_tightest() {
        assert_eq!(
            body("x = a ?? b ?? 0 + 1"),
            "(BinExpr (BinExpr (NameRef a) (BinExpr (NameRef b) (Literal 0))) (Literal 1))"
        );
        assert_eq!(body("x = f a ?? 0"), "(BinExpr (App (NameRef f) (NameRef a)) (Literal 0))");
    }
}
