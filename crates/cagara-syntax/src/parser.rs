use crate::lexer::{lex, Lexeme, Token};
use crate::ops::{ops, Ops, MAX_LEVEL, MIN_LEVEL};
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
    parse_with(input, ops())
}

/// Parse against an explicit operator table. The table says which spellings
/// are infix operators and how tightly they bind; `crate::ops` builds the
/// language's table from the prelude's declarations.
pub fn parse_with(input: &str, ops: &Ops) -> Parse {
    let mut p = Parser {
        toks: lex(input),
        pos: 0,
        b: GreenNodeBuilder::new(),
        errors: Vec::new(),
        end: input.len(),
        depth: 0,
        bailed: false,
        ops,
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
    /// Nesting depth of `expr`, so pathological input is rejected instead of
    /// overflowing the stack.
    depth: usize,
    /// Set once the depth limit has been reported in the current definition.
    /// The rest of it has been consumed, so every enclosing construct would
    /// otherwise add its own "expected ..." error while unwinding.
    bailed: bool,
    ops: &'a Ops,
}

/// Deepest expression nesting accepted. Well below what overflows the stack
/// on a default 8 MiB thread, and far beyond any hand-written query.
///
/// The budget has to cover every later walk of the AST, not just this parser.
/// A left-associative chain (`a + a + ...`) is one level of parser recursion
/// but a left-nested tree of that depth, so the type checker descends it
/// recursively. At 256 the checker ran out of stack in a binary built with the
/// default 8 MiB stack: a 200-operator chain aborted the process (`SIGABRT`)
/// instead of reporting a diagnostic. 128 leaves comfortable headroom and is
/// still far past anything a person writes.
///
/// Pipelines share this expression budget. Common filter chains are lowered
/// iteratively, so there is no separate pipeline-specific cap.
const MAX_DEPTH: usize = 128;

/// The keywords that introduce an operator declaration. There is no separate
/// form for the pipeline: `&` and `&+` are declared exactly like `+`.
fn is_op_decl_kw(text: &str) -> bool {
    matches!(text, "infixl" | "infixr")
}

/// A token shaped like an operator that no declaration claims.
fn unknown_op(spelling: &str) -> String {
    format!(
        "unknown operator `{spelling}`; declare it in `prelude.cagara`, e.g. \
         `infixl 1 {spelling}` for a pipeline stage (`&` is declared at level 1)"
    )
}

impl<'a> Parser<'a> {
    // ── token cursor ─────────────────────────────────────────

    /// Push leading trivia into the tree so it stays lossless.
    fn eat_trivia(&mut self) {
        while let Some(l) = self.toks.get(self.pos).copied() {
            if !l.kind.is_trivia() {
                break;
            }
            self.b
                .token(CagaraLanguage::kind_to_raw(l.kind.into()), l.text);
            self.pos += 1;
        }
    }

    fn peek_lex(&self, n: usize) -> Option<Lexeme<'a>> {
        self.toks[self.pos..]
            .iter()
            .filter(|l| !l.kind.is_trivia())
            .nth(n)
            .copied()
    }

    fn peek(&self) -> Option<Token> {
        self.peek_lex(0).map(|l| l.kind)
    }

    fn at(&self, t: Token) -> bool {
        self.peek() == Some(t)
    }

    /// `t` inside the current item: an identifier in column 0 starts the
    /// next one, so it is never a type, field, or alias of this one.
    fn at_inner(&self, t: Token) -> bool {
        self.at(t) && !self.at_boundary()
    }

    /// Tokens that close or separate an enclosing construct: recovery stops
    /// in front of them instead of swallowing them.
    fn at_closer(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::RParen | Token::RBrace | Token::RBracket | Token::Comma | Token::Eq)
        )
    }

    /// Enter one level of nesting; `false` when the limit is reached.
    ///
    /// The single gate for the expression/type budget. `expr`, `unary`, and the
    /// type grammar all enter through here, so the limit and the "report once"
    /// rule live in one place rather than in three copies that must stay in
    /// step. Only [`bin`](Parser::bin)'s chain counter raises `depth` by any
    /// other route, and it checks the same limit, so `depth` never exceeds
    /// `MAX_DEPTH`; `bail` consumes to the next item boundary, which is why the
    /// enclosing frames that unwind afterwards see `at_boundary()` and stay
    /// quiet instead of reporting again or eating the next item.
    fn descend(&mut self) -> bool {
        if self.depth >= MAX_DEPTH {
            if !self.at_boundary() {
                self.bail("expression is nested too deeply");
            }
            return false;
        }
        self.depth += 1;
        true
    }

    /// Layout rule: an identifier or `import` in column 0 starts a new item.
    fn at_boundary(&self) -> bool {
        match self.peek_lex(0) {
            None => true,
            Some(l) => l.line_start && matches!(l.kind, Token::Ident | Token::ImportKw),
        }
    }

    fn bump(&mut self) {
        self.eat_trivia();
        if let Some(l) = self.toks.get(self.pos).copied() {
            self.b
                .token(CagaraLanguage::kind_to_raw(l.kind.into()), l.text);
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
        if self.bailed {
            return;
        }
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

    /// Report the depth limit and consume the rest of the definition. Later
    /// errors in it are follow-on noise, so they are held back.
    fn bail(&mut self, message: &str) {
        self.recover(message);
        self.bailed = true;
    }

    // ── items ────────────────────────────────────────────────

    fn file(&mut self) {
        self.b
            .start_node(CagaraLanguage::kind_to_raw(K::SourceFile));
        loop {
            match self.peek() {
                None => break,
                Some(Token::ImportKw) => self.import(),
                Some(Token::Ident) if self.at_op_decl() => self.op_decl(),
                Some(Token::Ident) => self.def(),
                Some(_) => self.recover("expected a definition or import"),
            }
        }
        self.eat_trivia();
        self.finish();
    }

    /// An operator declaration in item position. The tokens `infixl` and
    /// `infixr` are reserved where a definition may start, so a line beginning
    /// with one is always a declaration — which is also why a malformed one
    /// says how to write it rather than "expected `=`".
    fn at_op_decl(&self) -> bool {
        self.peek_lex(0).is_some_and(|l| is_op_decl_kw(l.text))
    }

    /// `infixl 1 &+` / `infixr 21 ??`.
    fn op_decl(&mut self) {
        self.bailed = false;
        self.start(K::OpDecl);
        let keyword = self.peek_lex(0).map(|l| l.text).unwrap_or_default();
        self.bump(); // infixl / infixr
        let mut ok = true;
        match self.peek_lex(0).and_then(|l| l.text.parse::<u8>().ok()) {
            Some(n) if (MIN_LEVEL..=MAX_LEVEL).contains(&n) => {
                self.bump();
            }
            Some(_) => {
                self.error(format!(
                    "operator precedence must be between {MIN_LEVEL} and {MAX_LEVEL}"
                ));
                self.bump();
                ok = false;
            }
            None => {
                self.error("expected a precedence level, e.g. `infixl 1 &+`");
                ok = false;
            }
        }
        if self.peek().is_some_and(Token::is_op_symbol) {
            self.bump();
        } else if ok {
            // Only when the level was fine: one malformed declaration is one
            // diagnostic, not a complaint about each missing part.
            self.error(format!("expected an operator symbol after `{keyword}`"));
            ok = false;
        }
        if !ok {
            // Take the rest of the malformed item with it, so one bad
            // declaration is one diagnostic rather than a cascade.
            while self.peek().is_some() && !self.at_boundary() {
                self.bump();
            }
        }
        self.bailed = false;
        self.finish();
    }

    fn import(&mut self) {
        self.start(K::ImportDecl);
        self.bump();
        self.expect(Token::String, "import path string");
        if self.at(Token::AsKw) {
            self.bump();
            if self.at_inner(Token::Ident) {
                self.bump();
            } else {
                self.error("expected module alias");
            }
        }
        self.finish();
    }

    fn def(&mut self) {
        self.bailed = false;
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
        self.bailed = false;
        self.finish();
    }

    // ── types ────────────────────────────────────────────────

    fn ty(&mut self) {
        if !self.descend() {
            return;
        }
        let cp = self.checkpoint();
        self.ty_app();
        if self.at(Token::Arrow) {
            self.bump();
            self.ty();
            self.wrap(cp, K::TyFun);
        }
        self.depth -= 1;
    }

    fn ty_atom_start(&self) -> bool {
        !self.at_boundary()
            && matches!(
                self.peek(),
                Some(Token::Ident | Token::LBrace | Token::LParen | Token::String)
            )
    }

    /// `head arg arg`; a bare record or paren type is also accepted.
    fn ty_app(&mut self) {
        if self.at_inner(Token::Ident) {
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
        match self.peek().filter(|_| !self.at_boundary()) {
            Some(Token::Ident) => {
                self.start(K::TyApp);
                self.bump();
                self.finish();
            }
            // A string in type position: the affix of a key mapper, as in
            // `keyMap (prefix "u_") r`. Only meaningful there, and the checker
            // is what rejects it elsewhere.
            Some(Token::String) => {
                self.start(K::TyStr);
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
                while self.at_inner(Token::Ident) {
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
                    if self.at_inner(Token::Ident) {
                        self.bump();
                    } else {
                        self.error("expected row variable after `|`");
                    }
                }
                self.expect(Token::RBrace, "`}`");
                self.finish();
            }
            _ => {
                self.error("expected a type");
                self.start(K::ErrorNode);
                if !self.at_boundary() && !self.at_closer() && !self.at(Token::Arrow) {
                    self.bump();
                }
                self.finish();
            }
        }
    }

    // ── expressions ──────────────────────────────────────────

    fn at_lambda(&self) -> bool {
        self.at_inner(Token::Ident) && self.peek_lex(1).map(|l| l.kind) == Some(Token::FatArrow)
    }

    fn expr(&mut self) {
        // Bail out on runaway nesting: report once, then consume to the next
        // item boundary so the rest of the file still parses.
        if !self.descend() {
            return;
        }
        if self.at_lambda() {
            self.start(K::Lambda);
            self.bump(); // param
            self.bump(); // =>
            self.expr();
            self.finish();
        } else {
            self.bin(0);
        }
        self.depth -= 1;
    }

    /// Pratt loop over the operator table in `SyntaxKind::infix`.
    fn bin(&mut self, min_bp: u8) {
        let cp = self.checkpoint();
        self.unary();
        // A left-associative chain (`1 + 1 + 1 + ...`) stays at one level of
        // parser recursion but still deepens the AST, so count its length
        // against the same budget.
        let mut chain = 0usize;
        loop {
            if self.at_boundary() {
                break;
            }
            let Some(l) = self.peek_lex(0) else { break };
            let Some(op) = self.ops.find(l.text) else {
                // Report a spelling nothing declares here, where the token is
                // known to sit in operator position, instead of letting it
                // fall through to a generic "unexpected tokens" error.
                if l.kind.is_op_symbol() {
                    let msg = unknown_op(l.text);
                    self.bail(&msg);
                }
                break;
            };
            let (l_bp, r_bp) = op.precedence();
            if l_bp < min_bp {
                break;
            }
            chain += 1;
            if self.depth + chain >= MAX_DEPTH {
                self.bail("expression is too long");
                break;
            }
            self.bump();
            if self.at_lambda() {
                // allow `q & x => ...` style right operands
                self.expr();
            } else {
                self.depth += 1;
                self.bin(r_bp);
                self.depth -= 1;
            }
            self.wrap(cp, K::BinExpr);
        }
    }

    fn unary(&mut self) {
        if self.at(Token::Minus) {
            if !self.descend() {
                return;
            }
            self.start(K::NegExpr);
            self.bump();
            // `-9223372036854775808` is in range, though its digits alone
            // are not. The magnitude is shared with `ast`, which turns the
            // resulting error node back into `i64::MIN`.
            if self
                .peek_lex(0)
                .is_some_and(|l| l.text == crate::ast::I64_MIN_MAGNITUDE)
            {
                self.start(K::Literal);
                self.bump();
                self.finish();
            } else {
                self.unary();
            }
            self.finish();
            self.depth -= 1;
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
                        | Token::UnterminatedString
                        | Token::Field
                        | Token::LeftField
                        | Token::RightField
                        | Token::LParen
                        | Token::LBrace
                        | Token::LBracket
                        | Token::SqlKw
                        | Token::PrimitiveKw
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
                    // chain: a.b.c, each link one level deeper in the AST.
                    let mut chain = 0;
                    while self.toks.get(self.pos).map(|l| l.kind) == Some(Token::Field) {
                        chain += 1;
                        if self.depth + chain >= MAX_DEPTH {
                            self.bail("expression is too long");
                            break;
                        }
                        self.bump();
                        self.wrap(cp, K::ProjExpr);
                    }
                }
            }
            Some(Token::Int) => {
                let text = self.peek_lex(0).map_or("", |l| l.text);
                if text.parse::<i64>().is_err() {
                    self.error("integer literal is too large (the limit is 9223372036854775807)");
                }
                self.start(K::Literal);
                self.bump();
                self.finish();
            }
            Some(Token::Float | Token::String) => {
                self.start(K::Literal);
                self.bump();
                self.finish();
            }
            Some(Token::UnterminatedString) => {
                self.error("unterminated string: a string must end on the line it starts");
                self.start(K::Literal);
                self.bump();
                self.finish();
            }
            Some(Token::Field | Token::LeftField | Token::RightField) => {
                self.start(K::FieldExpr);
                self.bump();
                self.finish();
            }
            Some(Token::SqlKw) => {
                self.start(K::SqlExpr);
                self.bump();
                self.expect(Token::String, "SQL template string after `sql`");
                self.finish();
            }
            Some(Token::PrimitiveKw) => {
                self.start(K::PrimitiveExpr);
                self.bump();
                self.expect(Token::String, "primitive name after `primitive`");
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
            _ => match self.peek_lex(0) {
                // An operator in atom position (`q = &+ {..}` without a left
                // operand) is still an undeclared-operator problem, not a
                // missing expression.
                Some(l) if l.kind.is_op_symbol() && self.ops.find(l.text).is_none() => {
                    let msg = unknown_op(l.text);
                    self.bail(&msg);
                }
                _ => {
                    self.error("expected an expression");
                    self.start(K::ErrorNode);
                    if !self.at_closer() {
                        self.bump();
                    }
                    self.finish();
                }
            },
        }
    }

    fn record(&mut self) {
        self.start(K::RecordExpr);
        self.bump(); // {
                     // A field is `name = expr`, or the shorthand `.name` for
                     // `name = .name`, so `select {.id, .name}` chooses columns
                     // without naming each one twice.
        while self.at_inner(Token::Ident) || self.at(Token::Field) {
            self.start(K::RecordField);
            // `.name` is the shorthand; `name = expr` is the long form.
            let shorthand = self.at(Token::Field);
            self.bump();
            if !shorthand {
                self.expect(Token::Eq, "`=` in record");
                self.expr();
            }
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
                    if matches!(t.kind(), K::Ident | K::Int | K::Field) && n.kind() != K::Definition
                    {
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
        let e = def
            .children()
            .filter(|c| c.kind() != K::TypeAnn)
            .last()
            .unwrap();
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

    /// A line break is `\n`, `\r\n`, or a bare `\r`: the whitespace regex
    /// accepts all three, so the layout rule must recognise all three or a
    /// CR-only file's definitions merge into one.
    #[test]
    fn every_line_break_spelling_ends_a_definition() {
        for sep in ["\n", "\r\n", "\r"] {
            let src = format!("a = 1{sep}b = 2{sep}");
            let p = parse(&src);
            let defs: Vec<_> = p.syntax().children().collect();
            assert_eq!(defs.len(), 2, "{sep:?}: {:?}", p.errors);
        }
    }

    #[test]
    fn types_parse() {
        let p = parse("u : query { id = int, age = int | r } -> expr r bool = x");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let ann = p
            .syntax()
            .descendants()
            .find(|n| n.kind() == K::TypeAnn)
            .unwrap();
        assert!(ann.descendants().any(|n| n.kind() == K::TyFun));
        assert!(ann.descendants().any(|n| n.kind() == K::TyRecord));
    }

    #[test]
    fn recovery_and_lossless() {
        let src = "a = (1 +\nb = 2\n";
        let p = parse(src);
        assert!(!p.errors.is_empty());
        assert_eq!(
            p.syntax()
                .children()
                .filter(|n| n.kind() == K::Definition)
                .count(),
            2
        );
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

    /// `&+` is one token and chains like the other stage shorthands, so a
    /// record argument and a following shorthand both parse.
    #[test]
    fn update_shorthand_parses_like_its_siblings() {
        assert_eq!(
            body("q = a &+ {x = 1} &= {.x}"),
            "(BinExpr (BinExpr (NameRef a) (RecordExpr (RecordField x (Literal 1)))) (RecordExpr (RecordField .x)))"
        );
        assert_eq!(
            body("q = a & b + c"),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
    }

    #[test]
    fn coalesce_is_right_assoc_and_tightest() {
        assert_eq!(
            body("x = a ?? b ?? 0 + 1"),
            "(BinExpr (BinExpr (NameRef a) (BinExpr (NameRef b) (Literal 0))) (Literal 1))"
        );
        assert_eq!(
            body("x = f a ?? 0"),
            "(BinExpr (App (NameRef f) (NameRef a)) (Literal 0))"
        );
    }

    #[test]
    fn runaway_nesting_is_a_syntax_error_not_a_crash() {
        // Each of these used to overflow the stack while parsing or checking.
        // Each reports exactly one error: the enclosing constructs must not
        // add an "expected `)`" apiece while unwinding. The next definition
        // parses normally, and its own errors are still reported.
        let deep_parens = format!("x = {}1{}\ny = 2\n", "(".repeat(4000), ")".repeat(4000));
        let deep_neg = format!("x = {}1\ny = 2\n", "-".repeat(4000));
        let long_chain = format!("x = 1{}\ny = 2\n", " + 1".repeat(4000));
        for src in [deep_parens, deep_neg, long_chain] {
            let p = parse(&src);
            assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
            assert!(p.errors[0].message.contains("too "), "{:?}", p.errors);
            let later = parse(&format!("{src}z = (1\n"));
            assert_eq!(later.errors.len(), 2, "{:?}", later.errors);
            assert_eq!(later.errors[1].message, "expected `)`");
            // The tree stays lossless, so the editor can still work on it.
            assert_eq!(p.syntax().text().to_string(), src);
        }
    }

    /// A pipeline is one stage per `&`, so its AST is as deep as it is long.
    /// Pipelines use the shared expression nesting budget; there is no separate
    /// language-level stage count.
    #[test]
    fn a_pipeline_past_the_expression_budget_is_a_diagnostic_not_a_crash() {
        let stages = |n: usize| {
            let mut s = String::from("q = users");
            for i in 0..n {
                s.push_str(&format!(" & where (.age > {i})"));
            }
            s.push('\n');
            s
        };
        // Comfortably inside: accepted.
        assert!(parse(&stages(1)).errors.is_empty());
        assert!(parse(&stages(20)).errors.is_empty());
        // Past the shared expression budget: one diagnostic, not a crash.
        for n in [128, 256, 1000] {
            let src = stages(n);
            let p = parse(&src);
            assert_eq!(p.errors.len(), 1, "n={n}: {:?}", p.errors);
            assert!(
                p.errors[0].message.contains("too long")
                    || p.errors[0].message.contains("too deeply"),
                "n={n}: {:?}",
                p.errors
            );
            // The tree stays lossless, so the editor can still work on it.
            assert_eq!(p.syntax().text().to_string(), src);
        }
    }

    /// Pipeline shorthands use the same expression budget as the long form.
    #[test]
    fn pipeline_shorthands_share_the_expression_budget() {
        for op in ["&", "&?", "&=", "&+", "&*", "&.", "&-"] {
            let mut src = String::from("q = users");
            for i in 0..150 {
                src.push_str(&format!(" {op} (.age > {i})"));
            }
            src.push('\n');
            let p = parse(&src);
            assert_eq!(p.errors.len(), 1, "{op}: {:?}", p.errors);
            assert!(
                p.errors[0].message.contains("too long")
                    || p.errors[0].message.contains("too deeply"),
                "{op}: {:?}",
                p.errors
            );
        }
    }

    /// The real prelude's table, plus whatever this test declares on top.
    fn ops_with(extra: &str) -> Ops {
        Ops::with_declarations(&format!("{}{extra}", crate::ops::PRELUDE_SRC))
    }

    fn body_with(src: &str, ops: &Ops) -> String {
        let p = crate::parse_with(src, ops);
        assert!(p.errors.is_empty(), "errors: {:?}", p.errors);
        let def = p.syntax().children().next().unwrap();
        let e = def
            .children()
            .filter(|c| c.kind() != K::TypeAnn)
            .last()
            .unwrap();
        sexp(&e)
    }

    /// A declaration is the whole cost of a new operator: an undeclared
    /// spelling is a syntax error, and the same spelling becomes an operator
    /// of whatever precedence and associativity the declaration gives it.
    #[test]
    fn a_declaration_is_what_makes_an_operator_an_operator() {
        // Undeclared: rejected, and told how to fix it. The spellings must
        // stay undeclared by `prelude.cagara`: this test is about the
        // *absence* of a declaration, so it cannot use one the prelude has
        // since adopted (`&^` used to be free; it is now `intersect`).
        let p = parse("q = a &%% 5\n");
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(
            p.errors[0].message.contains("unknown operator `&%%`"),
            "{:?}",
            p.errors
        );
        assert!(
            p.errors[0].message.contains("infixl 1 &%%"),
            "{:?}",
            p.errors
        );
        // The same for a spelling outside the `&` family.
        let p = parse("q = a ~= b\n");
        assert!(
            p.errors[0].message.contains("unknown operator `~=`"),
            "{:?}",
            p.errors
        );

        // Declared at the pipe level, it is a stage: left-assoc and chaining
        // with the shorthands the prelude already declares.
        let ops = ops_with("infixl 1 &%%\n");
        assert_eq!(
            body_with("q = a &%% 5 &- 1", &ops),
            "(BinExpr (BinExpr (NameRef a) (Literal 5)) (Literal 1))"
        );

        // Declared left-associative, it groups to the left…
        let ops = ops_with("infixl 17 **\n");
        assert_eq!(
            body_with("q = a ** b ** c", &ops),
            "(BinExpr (BinExpr (NameRef a) (NameRef b)) (NameRef c))"
        );
        // …and right-associative to the right.
        let ops = ops_with("infixr 17 **\n");
        assert_eq!(
            body_with("q = a ** b ** c", &ops),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );

        // Precedence follows the declared level: at 1 it is a pipe, at 17 it
        // sits with `+` and so binds tighter than `&`.
        let ops = ops_with("infixl 17 **\n");
        assert_eq!(
            body_with("q = a & b ** c", &ops),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
    }

    /// Declaring a spelling the prelude already owns changes that operator's
    /// fixity rather than adding a second one.
    #[test]
    fn a_declaration_can_refix_a_builtin() {
        let ops = ops_with("infixr 17 +\n");
        assert_eq!(
            body_with("q = a + b + c", &ops),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
    }

    /// A malformed declaration is reported and does not become an operator.
    #[test]
    fn malformed_declarations_are_reported() {
        for (src, want) in [
            ("infixl 251 &^\n", "between 0 and"),
            ("infixl 1\n", "expected an operator symbol"),
            ("infixl &^\n", "expected a precedence level"),
            // `infixl` and `infixr` are reserved at item position.
            ("infixl = 1\n", "expected a precedence level"),
        ] {
            let p = parse(src);
            assert_eq!(p.errors.len(), 1, "{src:?}: {:?}", p.errors);
            assert!(
                p.errors[0].message.contains(want),
                "{src:?}: {:?}",
                p.errors
            );
        }
        // The reserved words are still ordinary names inside an expression,
        // and so is `stage`, which is not a declaration form.
        assert!(parse("q = 1 & infixl 2\n").errors.is_empty());
        assert_eq!(body("stage = 1"), "(Literal 1)");
        // Level 0 is a real level: `$` is declared there.
        assert!(parse("infixl 0 &^\n").errors.is_empty());
    }

    /// The function combinators bind where Haskell declares them, and that is
    /// not only a matter of taste: with composition looser than the
    /// comparisons, `toInt >>> toString == "5"` parsed as
    /// `toInt >>> (toString == "5")`.
    #[test]
    fn combinators_bind_the_way_haskell_declares_them() {
        for op in [">>>", "<<<"] {
            // Composition is tighter than every other operator…
            assert_eq!(
                body(&format!("q = a {op} b == c")),
                "(BinExpr (BinExpr (NameRef a) (NameRef b)) (NameRef c))",
                "{op}"
            );
            assert_eq!(
                body(&format!("q = a {op} b + c")),
                "(BinExpr (BinExpr (NameRef a) (NameRef b)) (NameRef c))",
                "{op}"
            );
            // …and right-associative.
            assert_eq!(
                body(&format!("q = a {op} b {op} c")),
                "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))",
                "{op}"
            );
            // The idiom still reads as it should: the composition is the
            // stage's argument, so it is inside `&`.
            assert_eq!(
                body(&format!("q = a & b {op} c")),
                "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))",
                "{op}"
            );
        }
        // `$` is the loosest, so it applies a whole pipeline-shaped thing, and
        // `&` sits just above it.
        assert_eq!(
            body("q = a $ b & c"),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
        assert_eq!(
            body("q = a & b $ c"),
            "(BinExpr (BinExpr (NameRef a) (NameRef b)) (NameRef c))"
        );
        assert_eq!(
            body("q = a $ b $ c"),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
    }

    #[test]
    fn deeply_nested_but_reasonable_input_still_parses() {
        // Well inside the limit, and far deeper than any real query.
        let src = format!("x = {}1{}\n", "(".repeat(100), ")".repeat(100));
        let p = parse(&src);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
    }

    fn defs(p: &Parse) -> Vec<String> {
        p.syntax()
            .children()
            .filter(|n| n.kind() == K::Definition)
            .map(|n| {
                n.first_token()
                    .map_or(String::new(), |t| t.text().to_string())
            })
            .collect()
    }

    fn messages(p: &Parse) -> Vec<&str> {
        p.errors.iter().map(|e| e.message.as_str()).collect()
    }

    #[test]
    fn strings_end_at_their_line() {
        // A missing quote used to swallow the rest of the file, or pair with
        // the next string and silently make a different program.
        for src in ["x = \"abc\ny = 1\nz = 2\n", "x = \"abc\ny = 1\nz = \"q\"\n"] {
            let p = parse(src);
            assert_eq!(messages(&p).len(), 1, "{:?}", p.errors);
            assert!(
                p.errors[0].message.contains("unterminated string"),
                "{:?}",
                p.errors
            );
            assert_eq!(defs(&p), ["x", "y", "z"], "{src:?}");
            assert_eq!(p.syntax().text().to_string(), src);
        }
        // Escaped quotes still work.
        assert!(parse("x = \"a\\\"b\"\n").errors.is_empty());
    }

    #[test]
    fn column_zero_names_are_never_part_of_the_item_above() {
        // Half-typed items: each used to take the next definition in.
        for (src, msg) in [
            ("x :\ny = 1\n", "expected a type"),
            ("x : query {\ny = 1\n", "expected `}`"),
            ("x = {\ny = 1\n", "expected `}`"),
            ("x = (\ny = 1\n", "expected an expression"),
            ("import \"a\" as\ny = 1\n", "expected module alias"),
            ("x =\ny => 1\n", "expected an expression"),
        ] {
            let p = parse(src);
            assert!(messages(&p).contains(&msg), "{src:?}: {:?}", p.errors);
            assert!(
                defs(&p).contains(&"y".to_string()),
                "{src:?}: {}",
                sexp(&p.syntax())
            );
            assert_eq!(p.syntax().text().to_string(), src);
        }
        // `y` itself parses: its errors, if any, are its own.
        let p = parse("x :\ny = 1\n");
        assert!(p.errors.iter().all(|e| e.offset <= 4), "{:?}", p.errors);
    }

    #[test]
    fn recovery_keeps_closing_delimiters() {
        for (src, want) in [
            ("x = ()\n", vec!["expected an expression"]),
            ("x : = 1\n", vec!["expected a type"]),
            ("x = [1, ]\n", vec![]),
            (
                "x = f (, 1)\n",
                vec![
                    "expected an expression",
                    "expected `)`",
                    "unexpected tokens after definition",
                ],
            ),
        ] {
            assert_eq!(messages(&parse(src)), want, "{src:?}");
        }
    }

    #[test]
    fn long_types_and_projections_are_errors_not_overflows() {
        let arrows = format!("x : {}a = 1\ny = 2\n", "a -> ".repeat(30_000));
        let parens = format!(
            "x : {}a{} = 1\ny = 2\n",
            "(".repeat(30_000),
            ")".repeat(30_000)
        );
        let records = format!(
            "x : {}a{} = 1\ny = 2\n",
            "{ f = ".repeat(30_000),
            " }".repeat(30_000)
        );
        let proj = format!("x = m{}\ny = 2\n", ".a".repeat(30_000));
        for src in [arrows, parens, records, proj] {
            let p = parse(&src);
            assert_eq!(
                p.errors.len(),
                1,
                "{:?}",
                &p.errors[..p.errors.len().min(3)]
            );
            assert!(p.errors[0].message.contains("too "), "{:?}", p.errors);
            assert_eq!(defs(&p), ["x", "y"]);
        }
    }

    #[test]
    fn integer_literals_must_fit() {
        assert_eq!(
            messages(&parse("x = 9223372036854775808\n")),
            ["integer literal is too large (the limit is 9223372036854775807)"]
        );
        assert!(parse("x = -9223372036854775808\n").errors.is_empty());
        assert!(!parse("x = 1 -9223372036854775808\n").errors.is_empty());
    }

    #[test]
    fn concat_is_looser_than_arithmetic() {
        assert_eq!(
            body("x = a <> b + c <> d"),
            "(BinExpr (NameRef a) (BinExpr (BinExpr (NameRef b) (NameRef c)) (NameRef d)))"
        );
        assert_eq!(
            body("x = a == b <> c"),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
    }
}
