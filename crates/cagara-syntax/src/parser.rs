use crate::lexer::{lex, Lexeme, Token};
use crate::ops::{ops, Ops};
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
/// are infix operators and how tightly they bind; [`crate::ops()`] supplies the
/// language's fixed table.
pub fn parse_with(input: &str, ops: &Ops) -> Parse {
    let mut p = Parser {
        toks: lex(input),
        pos: 0,
        b: GreenNodeBuilder::new(),
        errors: Vec::new(),
        end: input.len(),
        depth: 0,
        bailed: false,
        arm_depth: None,
        stop_at_in: false,
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
    /// The expression-nesting depth a `case` arm's condition sits at, while one
    /// is being parsed, so `cond => value` is not mistaken for a lambda (see
    /// [`Parser::at_lambda_operand`]). A *depth* rather than a flag because the
    /// exception is about the arm's own level: a lambda nested inside the
    /// condition — in parentheses, in an argument — is a lambda like any other.
    arm_depth: Option<usize>,
    /// While parsing a `let` binding's value, an identifier spelled `in` ends
    /// the value instead of being applied as an argument. `in` stays an
    /// ordinary identifier otherwise (the prelude defines it as an alias of
    /// `inList`), so this is a parse-context flag rather than a token kind.
    /// Delimited sub-expressions temporarily clear it, so `(in)` and
    /// `{ in = ... }` still use the name.
    stop_at_in: bool,
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

/// A token shaped like an operator that the fixed table does not contain.
///
/// Operators are part of the language, not something a source file can add;
/// the message says so and lists what is available, because the old wording
/// told readers to write an `infixl` line that the parser would then reject.
fn unknown_op(spelling: &str, table: &Ops) -> String {
    format!(
        "unknown operator `{spelling}`; Cagara's operator set is fixed and cannot be \
         extended from source. Known operators: {}",
        table.spellings().join(", ")
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
        // `min` rather than a plain index: `pos` can sit one past the last
        // token, and "no more tokens" should be an empty slice, not a panic.
        self.toks
            .get(self.pos.min(self.toks.len())..)
            .unwrap_or_default()
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

    /// A record or row-type field name.
    ///
    /// Field name is a position where only a name can appear (`name =` or
    /// `name = ty`), so a reserved word is an ordinary name there, exactly as
    /// it is inside an expression. Requiring `Ident` made a schema unable to
    /// describe a table with a `case` or `sql` column, and both are real
    /// columns in `icbase` (`gcm` 事由, `pbm` 查询语句). The word keeps its
    /// spelling, so the field name is the SQL column name and needs no escape.
    fn at_field_name(&self) -> bool {
        self.at_inner(Token::Ident)
            || matches!(
                self.peek(),
                Some(
                    Token::SqlKw
                        | Token::CaseKw
                        | Token::PrimitiveKw
                        | Token::ImportKw
                        | Token::AsKw
                        | Token::LetKw
                )
            )
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

    /// Layout rule: a top-level `name [':' type] '='` in column 0 starts a
    /// new item.
    ///
    /// A column-0 identifier that is not a definition header is a continuation
    /// the author forgot to indent. Treating it as part of the current item lets
    /// the parser recover and report that specifically; [`Parser::def`] does
    /// the reporting, because this method is asked on every type/field/atom
    /// boundary and must stay quiet.
    fn at_boundary(&self) -> bool {
        match self.peek_lex(0) {
            None => true,
            Some(l) if l.line_start => match l.kind {
                Token::ImportKw => true,
                Token::Ident => self.looks_like_definition(),
                _ => false,
            },
            Some(_) => false,
        }
    }

    /// Whether the identifier at the cursor looks like a definition header:
    /// `name = ...` or `name : type = ...`.
    ///
    /// The annotation is enough to recognise the header; the parser reports a
    /// missing `=` or type itself. Requiring the whole header here would make
    /// a half-typed definition swallow the following one, and scanning a long
    /// type on every boundary would be quadratic.
    fn looks_like_definition(&self) -> bool {
        matches!(
            self.peek_lex(1).map(|l| l.kind),
            Some(Token::Eq | Token::Colon)
        )
    }

    /// The contextual `in` that ends a `let` binding's value.
    fn at_in(&self) -> bool {
        self.peek_lex(0)
            .is_some_and(|l| l.kind == Token::Ident && l.text == "in")
    }

    /// Run a delimited sub-expression with the let-`in` terminator disabled.
    /// `(in)`, `{ in = ... }`, `[in]`, and `case { ... in ... }` then use the
    /// ordinary name while an `in` outside the delimiters still ends a `let`.
    fn without_in_stop<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.stop_at_in;
        self.stop_at_in = false;
        let out = f(self);
        self.stop_at_in = outer;
        out
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

    /// Report the first continuation line that starts in column 0.
    ///
    /// The parser accepts such a line as part of the current definition (the
    /// lookahead in [`Parser::at_boundary`] only ends an item at a real
    /// definition header), so the mistake is recoverable. It is still a layout
    /// error: only the first token of an item may sit in column 0.
    fn report_layout_continuation(&mut self, from: usize) {
        if self.bailed {
            return;
        }
        let end = self.pos.min(self.toks.len());
        let Some(toks) = self.toks.get(from.min(end)..end) else {
            return;
        };
        for l in toks {
            if l.kind.is_trivia() {
                continue;
            }
            if l.line_start && matches!(l.kind, Token::Ident | Token::ImportKw | Token::LetKw) {
                self.errors.push(ParseError {
                    offset: l.offset,
                    message: "a line starting with an identifier in column 0 begins a new \
                              definition; indent it to continue the previous definition"
                        .into(),
                });
                break;
            }
        }
    }

    // ── items ────────────────────────────────────────────────

    fn file(&mut self) {
        self.b
            .start_node(CagaraLanguage::kind_to_raw(K::SourceFile));
        loop {
            match self.peek() {
                None => break,
                // A top-level `;` is an optional item terminator; a stray one
                // between items is harmless.
                Some(Token::Semicolon) => self.bump(),
                Some(Token::ImportKw) => self.import(),
                Some(Token::Ident) if self.looks_like_definition() => self.def(),
                Some(Token::Ident) => {
                    self.recover("expected a definition (`name = ...`) or `import` at column 0")
                }
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
        if self.at(Token::AsKw) {
            self.bump();
            if self.at_inner(Token::Ident) {
                self.bump();
            } else {
                self.error("expected module alias");
            }
        }
        if self.at(Token::Semicolon) {
            self.bump();
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
            let body_start = self.pos;
            self.expr();
            if !self.at(Token::Semicolon) && !self.at_boundary() {
                self.recover("unexpected tokens after definition");
            }
            self.report_layout_continuation(body_start);
        }
        // An optional terminator. The formatter does not print it; it exists
        // so generated code can close an item without relying on indentation.
        if self.at(Token::Semicolon) {
            self.bump();
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
                while self.at_field_name() {
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

    /// Like [`at_lambda`](Parser::at_lambda), but not at the start of a `case`
    /// arm. `cond => value` and `p => body` are the same two tokens, so
    /// without this an arm's condition would parse as a lambda parameter and
    /// the arm's own `=>` would then be missing — every arm of every `case`
    /// would be a syntax error. An arm's *value* is parsed normally, so a
    /// lambda is still a term like any other one `=>` away.
    fn at_lambda_operand(&self) -> bool {
        !self.at_arm() && self.at_lambda()
    }

    /// Is the cursor at the arm's **own** level of its condition?
    ///
    /// `arm` records the depth it sits at, and the condition's own `expr` frame
    /// is one deeper than that (it is the frame `descend` charges), so equality
    /// with `arm_depth + 1` is "at the arm's level" and anything deeper is a
    /// nested construct, where `=>` means a lambda again.
    fn at_arm(&self) -> bool {
        self.arm_depth.is_some_and(|d| self.depth == d + 1)
    }

    fn expr(&mut self) {
        // Bail out on runaway nesting: report once, then consume to the next
        // item boundary so the rest of the file still parses.
        if !self.descend() {
            return;
        }
        if self.at_lambda_operand() {
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
                    let msg = unknown_op(l.text, self.ops);
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
            if self.at_lambda_operand() {
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

    /// `let name = value in body`.
    ///
    /// `in` is a contextual identifier: only the value position stops at it.
    /// The AST lowering desugars the form to `(name => body) value`, so the
    /// checker, normalizer, and SQL backend need no new expression case.
    fn let_expr(&mut self) {
        self.start(K::LetExpr);
        self.bump(); // let
        if self.at_inner(Token::Ident) {
            self.bump();
        } else {
            self.error("expected a name after `let`");
        }
        self.expect(Token::Eq, "`=` after a let binding's name");
        let outer = self.stop_at_in;
        self.stop_at_in = true;
        self.expr();
        self.stop_at_in = outer;
        if self.at_in() {
            self.bump();
        } else {
            self.error("expected `in` after the let binding's value");
        }
        self.expr();
        self.finish();
    }

    fn atom_start(&self) -> bool {
        if self.stop_at_in && self.at_in() {
            return false;
        }
        !self.at_boundary()
            && !self.at_lambda_operand()
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
                        | Token::CaseKw
                        | Token::PrimitiveKw
                        | Token::LetKw
                )
            )
    }

    /// `f a b` — juxtaposition; a single atom is not wrapped.
    ///
    /// The number of arguments is budgeted the way a binary chain is. The tree
    /// is **flat** here — one `App` node however many atoms — so `depth` never
    /// rises and the limit that catches nested expressions does not apply. A
    /// flat application of a few thousand atoms then cost the checker minutes
    /// and, past that, overflowed its stack: `id id id … 1` is not a program
    /// anyone writes, and a budget says so instead of dying on it.
    fn app(&mut self) {
        let cp = self.checkpoint();
        self.atom();
        let mut applied = false;
        let mut chain = 0usize;
        while self.atom_start() {
            chain += 1;
            if self.depth + chain >= MAX_DEPTH {
                self.bail("application is too long");
                break;
            }
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
        if self.stop_at_in && self.at_in() {
            self.error("expected the let binding's value before `in`");
            self.start(K::ErrorNode);
            self.finish();
            return;
        }
        if self.at_boundary() {
            self.error("expected an expression");
            self.start(K::ErrorNode);
            self.finish();
            return;
        }
        match self.peek() {
            Some(Token::LetKw) => self.let_expr(),
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
            Some(Token::CaseKw) => self.case_expr(),
            Some(Token::PrimitiveKw) => {
                self.start(K::PrimitiveExpr);
                self.bump();
                self.expect(Token::String, "primitive name after `primitive`");
                self.finish();
            }
            Some(Token::LParen) => {
                self.start(K::ParenExpr);
                self.bump();
                self.without_in_stop(|p| p.expr());
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
                    let msg = unknown_op(l.text, self.ops);
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
        let outer = self.stop_at_in;
        self.stop_at_in = false;
        self.bump(); // {
                     // A field is `name = expr`, or the shorthand `.name` for
                     // `name = .name`, so `select {.id, .name}` chooses columns
                     // without naming each one twice.
        while self.at_field_name() || self.at(Token::Field) {
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
        self.stop_at_in = outer;
        self.finish();
    }

    fn list(&mut self) {
        self.start(K::ListExpr);
        let outer = self.stop_at_in;
        self.stop_at_in = false;
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
        self.stop_at_in = outer;
        self.finish();
    }

    /// `case { cond => value; cond => value; ... }` — a multi-way
    /// `ifThenElse`. Braces and semicolons only: a `case` has to be usable
    /// inside a record (`select { x = case { ... } }`), so it must not share
    /// the comma or the closing brace with the record around it.
    ///
    /// Whether the arms type-check — that they agree on a result type, and
    /// that the last one is `true =>` — is the checker's question, not the
    /// parser's. Here an arm is only required to have the shape
    /// `expr => expr`.
    fn case_expr(&mut self) {
        self.start(K::CaseExpr);
        let outer = self.stop_at_in;
        self.stop_at_in = false;
        self.bump(); // case
        if !self.expect(Token::LBrace, "`{` after `case`") {
            self.stop_at_in = outer;
            self.finish();
            return;
        }
        loop {
            // A name in column 0 ends the definition, so it is never an arm.
            if self.at(Token::RBrace) || self.at_boundary() {
                break;
            }
            self.arm();
            if self.at(Token::Semicolon) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(Token::RBrace, "`}` closing `case`");
        self.stop_at_in = outer;
        self.finish();
    }

    /// One `cond => value` arm. The separator is `=>`, the same token a lambda
    /// uses, so no new punctuation is introduced — which is why the condition is
    /// parsed with `arm_depth` set: at the arm's own level it must not be
    /// treated as a lambda's parameter list.
    ///
    /// The marker is saved and restored rather than cleared, because an arm may
    /// contain another `case` — including in its condition — and the inner arm
    /// must not leave the outer one unmarked.
    fn arm(&mut self) {
        self.start(K::CaseArm);
        let outer = self.arm_depth;
        self.arm_depth = Some(self.depth);
        self.expr();
        self.arm_depth = outer;
        if self.expect(
            Token::FatArrow,
            "`=>` between a case's condition and its value",
        ) {
            self.expr();
        }
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    // Test code may assert with `unwrap`, `expect`, and indexing: a panic
    // here is a failing test, not a broken compiler.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

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
    fn let_is_an_expression() {
        assert_eq!(
            body("x = let y = 1 in y + 2"),
            "(LetExpr y (Literal 1) in (BinExpr (NameRef y) (Literal 2)))"
        );
        assert_eq!(
            body("x = let f = y => y + 1 in f .a"),
            "(LetExpr f (Lambda y (BinExpr (NameRef y) (Literal 1))) in (App (NameRef f) (FieldExpr .a)))"
        );
        // `in` remains an ordinary name outside the binding's value.
        assert_eq!(
            body("x = in [1, 2] .a"),
            "(App (NameRef in) (ListExpr (Literal 1) (Literal 2)) (FieldExpr .a))"
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

    /// A top-level `;` is an optional item terminator.
    #[test]
    fn a_top_level_semicolon_terminates_an_item() {
        let p = parse("a = 1; b = 2;\n");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(defs(&p), ["a", "b"]);
        let p = parse("import \"schema.cagara\" as s;\na = s.users\n");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(defs(&p), ["a"]);
        // A stray separator between items is harmless.
        let p = parse("a = 1\n;\nb = 2\n");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(defs(&p), ["a", "b"]);
    }

    /// An unindented continuation is parsed as part of the current item and
    /// reported as a layout error, rather than silently becoming a new
    /// definition or being swallowed by recovery.
    #[test]
    fn an_unindented_continuation_is_reported() {
        let src = "a = users\nwhere (.age > 1)\nb = 2\n";
        let p = parse(src);
        assert_eq!(defs(&p), ["a", "b"], "{}", sexp(&p.syntax()));
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(p.errors[0].message.contains("indent"), "{:?}", p.errors);
        assert_eq!(p.syntax().text().to_string(), src);
    }

    /// `case { cond => value; ... }`: arms separated by `;`, the whole thing
    /// delimited by braces so it can sit inside a record.
    #[test]
    fn case_arms_are_semicolon_separated() {
        assert_eq!(
            body("x = case { a => 1; b => 2 }"),
            "(CaseExpr (CaseArm (NameRef a) (Literal 1)) (CaseArm (NameRef b) (Literal 2)))"
        );
        // One arm, and a trailing separator.
        assert_eq!(
            body("x = case { .a => 1; }"),
            "(CaseExpr (CaseArm (FieldExpr .a) (Literal 1)))"
        );
        // An empty `case` parses; whether it has an `else` is the checker's
        // question, so the parser does not turn it into a syntax error.
        assert_eq!(body("x = case {}"), "(CaseExpr)");
    }

    /// A lambda nested inside a `case` arm's condition is a lambda.
    ///
    /// The arm/`=>` exception is about the arm's **own** level: `cond => value`
    /// and `p => body` are the same two tokens, so at that level the first `=>`
    /// belongs to the arm. The marker used to apply to the whole condition
    /// subtree instead, so a lambda one level down — in parentheses, in an
    /// argument — was read as an arm condition and the arm's own `=>` was then
    /// reported missing.
    #[test]
    fn a_lambda_inside_a_case_condition_is_a_lambda() {
        assert_eq!(
            body("x = case { f (y => 1) => 2; true => 3 }"),
            "(CaseExpr (CaseArm (App (NameRef f) (ParenExpr (Lambda y (Literal 1)))) (Literal 2)) (CaseArm (NameRef true) (Literal 3)))"
        );
        assert_eq!(
            body("x = case { (y => 1) => 2; true => 3 }"),
            "(CaseExpr (CaseArm (ParenExpr (Lambda y (Literal 1))) (Literal 2)) (CaseArm (NameRef true) (Literal 3)))"
        );
    }

    /// An arm may hold another `case`, including in its own condition.
    ///
    /// The marker used to be a plain flag cleared on the way out of an inner
    /// arm, so the outer arm was left unmarked and its `=>` was parsed as a
    /// lambda's — `case { case { true => 1 } & b => c }` lost its arm.
    #[test]
    fn a_nested_case_keeps_the_outer_arms_marker() {
        assert_eq!(
            body("x = case { case { true => 1 } & b => c }"),
            "(CaseExpr (CaseArm (BinExpr (CaseExpr (CaseArm (NameRef true) (Literal 1))) (NameRef b)) (NameRef c)))"
        );
    }

    /// The reason for `{ ... ; ... }` rather than a comma-separated form: a
    /// `case` is a term like any other, so it nests inside a record, an
    /// argument list, and another `case`. Sharing the comma would make
    /// `{ x = case { a => 1, b => 2 }, y = 3 }` ambiguous.
    #[test]
    fn case_nests_inside_records_and_arguments() {
        assert_eq!(
            body("q = t & select { x = case { .a => 1; .b => 2 }, y = 3 }"),
            "(BinExpr (NameRef t) (App (NameRef select) (RecordExpr (RecordField x (CaseExpr (CaseArm (FieldExpr .a) (Literal 1)) (CaseArm (FieldExpr .b) (Literal 2)))) (RecordField y (Literal 3)))))"
        );
        // As an argument. The parens are `ParenExpr` in the tree, which is
        // what keeps `f (case {..}) 3` an application of two arguments.
        assert_eq!(
            body("x = f (case { a => 1; b => 2 }) 3"),
            "(App (NameRef f) (ParenExpr (CaseExpr (CaseArm (NameRef a) (Literal 1)) (CaseArm (NameRef b) (Literal 2)))) (Literal 3))"
        );
        assert_eq!(
            body("x = f (case { a => 1; true => 2 }) 3"),
            "(App (NameRef f) (ParenExpr (CaseExpr (CaseArm (NameRef a) (Literal 1)) (CaseArm (NameRef true) (Literal 2)))) (Literal 3))"
        );
        // A `case` is an atom wherever an atom may stand: `{` follows it
        // immediately, so no projection is formed and it needs no parens.
        // `x` is applied to `.v` — the same rule that makes `f .x` an
        // application rather than a projection.
        assert_eq!(
            body("x = f case { a => 1; true => 2 }.v"),
            "(App (NameRef f) (CaseExpr (CaseArm (NameRef a) (Literal 1)) (CaseArm (NameRef true) (Literal 2))) (FieldExpr .v))"
        );
        // The value side is parsed normally, so a lambda is still a lambda
        // there — only the condition is protected from `at_lambda`.
        assert_eq!(
            body("x = case { .a => y => y + 1; true => 2 }"),
            "(CaseExpr (CaseArm (FieldExpr .a) (Lambda y (BinExpr (NameRef y) (Literal 1)))) (CaseArm (NameRef true) (Literal 2)))"
        );
    }

    /// A lambda is still a lambda in an arm, and a missing value is one
    /// syntax error rather than a cascade.
    #[test]
    fn case_arms_report_their_own_mistakes() {
        let p = parse("x = case { a => 1; b }");
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(p.errors[0].message.contains("`=>`"), "{:?}", p.errors);
        let p = parse("x = case { a => 1");
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(p.errors[0].message.contains('}'), "{:?}", p.errors);
    }

    /// `case` always needs its braces: without them the arms would be
    /// juxtaposed with whatever follows, and the tree would not say what was
    /// meant.
    #[test]
    fn case_without_braces_is_a_syntax_error() {
        let p = parse("x = case a => 1");
        assert!(!p.errors.is_empty());
        assert!(p.errors[0].message.contains('{'), "{:?}", p.errors);
    }

    /// A name in column 0 still starts the next definition, even inside an
    /// unterminated `case`: the layout rule is what ends an item, so recovery
    /// must not swallow the rest of the file.
    #[test]
    fn an_unterminated_case_stops_at_the_next_definition() {
        let src = "a = case { x => 1\nb = 2\n";
        let p = parse(src);
        let defs: Vec<_> = p.syntax().children().collect();
        assert_eq!(defs.len(), 2, "{:?}", p.errors);
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

    /// A reserved word is an ordinary field name.
    ///
    /// A schema has to describe the table it names, and real tables have
    /// columns called `case` (`gcm` 事由) and `sql` (`pbm` 查询语句). A field
    /// name is a position where only a name can appear, so there is nothing
    /// for the word to be confused with: the parser keeps its spelling and the
    /// SQL backend quotes it when the dialect reserves it.
    #[test]
    fn a_reserved_word_is_a_field_name() {
        for word in ["case", "sql", "import", "as", "primitive"] {
            let src = format!("t : query {{ {word} = string }} = table \"s\" \"t\"\n");
            let p = parse(&src);
            assert!(p.errors.is_empty(), "{word} in a row type: {:?}", p.errors);

            let src = format!("q = t & select {{ {word} = 1 }}\n");
            let p = parse(&src);
            assert!(p.errors.is_empty(), "{word} in a record: {:?}", p.errors);
        }
        // The word still introduces its own form where that form is expected.
        assert!(parse("q = case { true => 1 }\n").errors.is_empty());
        assert!(parse("q = 1\nr : expr r int = sql \"1\"\n")
            .errors
            .is_empty());
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

    /// A flat application is budgeted like a binary chain.
    ///
    /// `f a b c …` is **one** `App` node however many atoms it holds, so the
    /// nesting budget never rises and nothing else bounded it. The checker then
    /// spent minutes on a few thousand atoms and overflowed its stack past that;
    /// `id id id … 1` is not a program anyone writes, and the budget says so
    /// rather than dying on it.
    #[test]
    fn an_application_past_the_expression_budget_is_a_diagnostic_not_a_crash() {
        let applied = |n: usize| format!("q = {}1\n", "id ".repeat(n));
        // Comfortably inside: accepted.
        assert!(parse(&applied(1)).errors.is_empty());
        assert!(parse(&applied(20)).errors.is_empty());
        // Past the shared expression budget: one diagnostic, not a hang.
        for n in [128, 1000, 5000] {
            let src = applied(n);
            let p = parse(&src);
            assert_eq!(p.errors.len(), 1, "n={n}: {:?}", p.errors);
            assert!(
                p.errors[0].message.contains("too long"),
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

    /// The operator table is fixed, so a spelling it does not carry cannot be
    /// used as an operator however it is written — and there is nothing a
    /// source file could write to change that.
    #[test]
    fn an_undeclared_spelling_is_not_an_operator() {
        // Rejected, and the message names the table it would have to be in.
        // The spelling must stay outside the table, so `&%%` is chosen from
        // the `&` family the table does not claim.
        let p = parse("q = a &%% 5\n");
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(
            p.errors[0].message.contains("unknown operator `&%%`"),
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

        // `infixl` / `infixr` are ordinary names now, not declarations: a line
        // that reads like one is a definition, so it cannot add an operator.
        let p = parse("infixl 1 &%%\n");
        assert!(
            !p.errors.is_empty(),
            "a declaration was accepted: a table entry is the only way"
        );
        assert!(parse("q = a &%% 5\n").errors[0].message.contains("&%%"));
    }

    /// The fixities in the table are the ones the grammar has always had, so
    /// removing the prelude's declarations changed nothing about how an
    /// expression parses.
    #[test]
    fn the_fixed_table_groups_expressions_as_before() {
        let ops = Ops::default();
        // Left-associative.
        assert_eq!(
            body_with("q = a + b + c", &ops),
            "(BinExpr (BinExpr (NameRef a) (NameRef b)) (NameRef c))"
        );
        // Right-associative: `>>>` is composition.
        assert_eq!(
            body_with("q = f >>> g >>> h", &ops),
            "(BinExpr (NameRef f) (BinExpr (NameRef g) (NameRef h)))"
        );
        // Precedence: `*` binds tighter than `+`.
        assert_eq!(
            body_with("q = a + b * c", &ops),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
        // The pipeline is looser than every expression operator, so its
        // operand is the whole expression.
        assert_eq!(
            body_with("q = a & b + c", &ops),
            "(BinExpr (NameRef a) (BinExpr (NameRef b) (NameRef c)))"
        );
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
        for src in [
            "x = \"abc\ry = 1\r",
            "x = \"abc\\\ny = 1\n",
            "x = \"abc\\\ry = 1\r",
        ] {
            let p = parse(src);
            assert_eq!(messages(&p).len(), 1, "{src:?}: {:?}", p.errors);
            assert_eq!(defs(&p), ["x", "y"], "{src:?}");
            assert_eq!(p.syntax().text().to_string(), src, "{src:?}");
        }
        // Escaped quotes still work.
        assert!(parse("x = \"a\\\"b\"\n").errors.is_empty());
    }

    #[test]
    fn column_zero_definition_headers_are_never_part_of_the_item_above() {
        // Half-typed items: each used to take the next definition in.
        for (src, msg) in [
            ("x :\ny = 1\n", "expected a type"),
            ("x : query {\ny = 1\n", "expected `}`"),
            ("x = {\ny = 1\n", "expected `}`"),
            ("x = (\ny = 1\n", "expected an expression"),
            ("import \"a\" as\ny = 1\n", "expected module alias"),
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
        // A column-0 name that is not a definition header is a continuation:
        // it is parsed as the body, and reported as a layout error so the
        // mistake is still visible. This is what lets a missing indent recover
        // without swallowing the next actual definition.
        let src = "x =\ny => 1\n";
        let p = parse(src);
        assert!(
            messages(&p).iter().any(|m| m.contains("indent")),
            "{:?}",
            p.errors
        );
        assert_eq!(defs(&p), ["x"], "{}", sexp(&p.syntax()));
        assert_eq!(p.syntax().text().to_string(), src);
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
