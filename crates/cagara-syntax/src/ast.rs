//! Owned AST lowered from the lossless rowan tree. Operators are desugared
//! into applications of their operator names (`a + b` => `_+_ a b`), so the
//! core has no built-in knowledge of arithmetic or pipelines.

use crate::ops::{op_name, Fixity, Ops};
use crate::{ops, parse_with, ParseError, SyntaxKind as K, SyntaxNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Module {
    pub imports: Vec<Import>,
    pub defs: Vec<Def>,
    /// `infixl` / `infixr` / `stage` declarations, which give operators their
    /// fixity. Only the prelude's are honoured (see `crate::ops`); elsewhere
    /// they are reported by `cagara-hir`.
    pub operators: Vec<OpDecl>,
}

/// One `infixl` / `infixr` line. There is no separate "stage" form: an
/// operator is a pipeline link because of where it is declared, not because of
/// how it is declared (see `crate::ops::Ops::is_stage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpDecl {
    pub spelling: String,
    pub fixity: Fixity,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub path: String,
    pub alias: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    /// `head arg...`; a lowercase head that is not a known constructor is a
    /// type variable (decided by the checker, not the parser).
    App {
        head: String,
        args: Vec<TypeExpr>,
        span: Span,
    },
    Record {
        fields: Vec<(String, TypeExpr)>,
        tail: Option<String>,
        span: Span,
    },
    Fun(Box<TypeExpr>, Box<TypeExpr>),
    Error(Span),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Single,
    Left,
    Right,
}

/// Unique within one module; the checker pairs it with a file id.
pub type ExprId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub id: ExprId,
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lit {
    Int(i64),
    /// Kept as source text so the AST stays `Eq` (salsa backdating).
    Float(String),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Name(String),
    Lit(Lit),
    /// `.x`, `.<x`, `.>x`
    Field(Side, String),
    /// `e.x` — module-qualified name or record projection (resolved later)
    Proj(Box<Expr>, String),
    /// Application arguments: usually one or two (operators desugar into
    /// applications), so they live behind one allocation as a boxed slice.
    App(Box<Expr>, Box<[Expr]>),
    Lambda(String, Box<Expr>),
    Record(Vec<(String, Expr)>),
    List(Vec<Expr>),
    /// `sql "template"` with `$1`, `$2` placeholders
    Sql(String),
    Error,
}

/// Parse and lower a source file with the language's operator table (the
/// built-ins plus the prelude's declarations). Parse errors are returned
/// alongside a best-effort module (erroneous parts become `ExprKind::Error`).
pub fn lower_source(src: &str) -> (Module, Vec<ParseError>) {
    lower_with(src, ops())
}

/// Like [`lower_source`], against an explicit operator table. Used to parse
/// the prelude while that table is still being built.
pub fn lower_with(src: &str, ops: &Ops) -> (Module, Vec<ParseError>) {
    let parse = parse_with(src, ops);
    let mut l = Lower { next: 0, ops };
    let module = l.module(&parse.syntax());
    (module, parse.errors)
}

struct Lower<'a> {
    next: ExprId,
    ops: &'a Ops,
}

fn span(n: &SyntaxNode) -> Span {
    let r = n.text_range();
    Span {
        start: r.start().into(),
        end: r.end().into(),
    }
}

fn token(n: &SyntaxNode, k: K) -> Option<crate::SyntaxToken> {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == k)
}

/// The text of a string literal, with its escapes resolved.
///
/// Only `\n`, `\t`, `\\` and `\"` are escapes. Every other backslash is kept
/// verbatim, backslash and all: `"a\\b"` is `a\b`, a Windows path or a regex
/// survives as written, and a trailing `\` does not vanish. Dropping the
/// backslash instead would silently change the value — `sql "... '\\'"` used
/// to lose it and then fail to parse, and `"... 'x\\ty'"` used to turn into a
/// literal tab.
fn unescape(s: &str) -> String {
    let inner = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            // Not an escape the language defines: keep it as written rather
            // than guessing, so text and AST cannot disagree.
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The magnitude of `-9223372036854775808`, the one negative literal whose
/// digits do not fit in an `i64`.
const I64_MIN_MAGNITUDE: &str = "9223372036854775808";

impl Lower<'_> {
    fn mk(&mut self, span: Span, kind: ExprKind) -> Expr {
        let id = self.next;
        self.next += 1;
        Expr { id, span, kind }
    }

    fn module(&mut self, root: &SyntaxNode) -> Module {
        let mut m = Module::default();
        for n in root.children() {
            match n.kind() {
                K::ImportDecl => {
                    if let Some(path) = token(&n, K::String) {
                        m.imports.push(Import {
                            path: unescape(path.text()),
                            alias: token(&n, K::Ident).map(|t| t.text().to_string()),
                            span: span(&n),
                        });
                    }
                }
                K::Definition => {
                    if let Some(d) = self.def(&n) {
                        m.defs.push(d);
                    }
                }
                K::OpDecl => {
                    if let Some(d) = self.op_decl(&n) {
                        m.operators.push(d);
                    }
                }
                _ => {}
            }
        }
        m
    }

    /// `infixl 1 &+` / `infixr 21 ??`: a level and a spelling.
    ///
    /// A declaration the parser rejected is dropped rather than recorded, so a
    /// malformed one never takes effect: whatever the parser complained about
    /// (a missing level, a spelling that is not an operator) has already been
    /// reported, and an out-of-range level is refused here for the same
    /// reason.
    fn op_decl(&mut self, n: &SyntaxNode) -> Option<OpDecl> {
        let keyword = token(n, K::Ident)?.text().to_string();
        let spelling = n
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind().is_op_symbol())?
            .text()
            .to_string();
        let level = token(n, K::Int)
            .and_then(|t| t.text().parse::<u8>().ok())
            .filter(|n| (crate::ops::MIN_LEVEL..=crate::ops::MAX_LEVEL).contains(n))?;
        let fixity = match keyword.as_str() {
            "infixl" => Fixity::left(level),
            "infixr" => Fixity::right(level),
            _ => return None,
        };
        Some(OpDecl {
            spelling,
            fixity,
            span: span(n),
        })
    }

    fn def(&mut self, n: &SyntaxNode) -> Option<Def> {
        let name = token(n, K::Ident)?.text().to_string();
        let ty = n
            .children()
            .find(|c| c.kind() == K::TypeAnn)
            .and_then(|a| a.children().next())
            .map(|t| self.ty(&t));
        let body = match n
            .children()
            .find(|c| !matches!(c.kind(), K::TypeAnn | K::ErrorNode))
        {
            Some(b) => self.expr(&b),
            None => self.mk(span(n), ExprKind::Error),
        };
        Some(Def {
            name,
            ty,
            body,
            span: span(n),
        })
    }

    fn ty(&mut self, n: &SyntaxNode) -> TypeExpr {
        match n.kind() {
            K::TyApp => match token(n, K::Ident) {
                Some(head) => TypeExpr::App {
                    head: head.text().to_string(),
                    args: n.children().map(|c| self.ty(&c)).collect(),
                    span: span(n),
                },
                None => TypeExpr::Error(span(n)),
            },
            K::TyRecord => {
                let fields = n
                    .children()
                    .filter(|c| c.kind() == K::TyField)
                    .filter_map(|f| {
                        let name = token(&f, K::Ident)?.text().to_string();
                        let t = f.children().next().map(|t| self.ty(&t))?;
                        Some((name, t))
                    })
                    .collect();
                // a direct Ident token child of the record is the `| tail`
                let tail = token(n, K::Ident).map(|t| t.text().to_string());
                TypeExpr::Record {
                    fields,
                    tail,
                    span: span(n),
                }
            }
            K::TyFun => {
                let mut cs = n.children();
                match (cs.next(), cs.next()) {
                    (Some(a), Some(b)) => {
                        TypeExpr::Fun(Box::new(self.ty(&a)), Box::new(self.ty(&b)))
                    }
                    _ => TypeExpr::Error(span(n)),
                }
            }
            K::TyParen => match n.children().next() {
                Some(c) => self.ty(&c),
                None => TypeExpr::Error(span(n)),
            },
            _ => TypeExpr::Error(span(n)),
        }
    }

    fn expr(&mut self, n: &SyntaxNode) -> Expr {
        let sp = span(n);
        let kind = match n.kind() {
            K::NameRef => match token(n, K::Ident).map(|t| t.text().to_string()) {
                Some(s) if s == "true" => ExprKind::Lit(Lit::Bool(true)),
                Some(s) if s == "false" => ExprKind::Lit(Lit::Bool(false)),
                Some(s) => ExprKind::Name(s),
                None => ExprKind::Error,
            },
            K::Literal => {
                let t = n
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| matches!(t.kind(), K::Int | K::Float | K::String));
                match t {
                    Some(t) if t.kind() == K::Int => match t.text().parse() {
                        Ok(v) => ExprKind::Lit(Lit::Int(v)),
                        Err(_) => ExprKind::Error,
                    },
                    Some(t) if t.kind() == K::Float => {
                        ExprKind::Lit(Lit::Float(t.text().to_string()))
                    }
                    Some(t) => ExprKind::Lit(Lit::Str(unescape(t.text()))),
                    None => ExprKind::Error,
                }
            }
            K::FieldExpr => {
                let t = n
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| matches!(t.kind(), K::Field | K::LeftField | K::RightField));
                match t {
                    Some(t) if t.kind() == K::LeftField => {
                        ExprKind::Field(Side::Left, t.text()[2..].to_string())
                    }
                    Some(t) if t.kind() == K::RightField => {
                        ExprKind::Field(Side::Right, t.text()[2..].to_string())
                    }
                    Some(t) => ExprKind::Field(Side::Single, t.text()[1..].to_string()),
                    None => ExprKind::Error,
                }
            }
            K::ProjExpr => match (n.children().next(), token(n, K::Field)) {
                (Some(inner), Some(f)) => {
                    ExprKind::Proj(Box::new(self.expr(&inner)), f.text()[1..].to_string())
                }
                _ => ExprKind::Error,
            },
            K::App => {
                let mut cs: Vec<Expr> = n.children().map(|c| self.expr(&c)).collect();
                if cs.is_empty() {
                    ExprKind::Error
                } else {
                    let head = cs.remove(0);
                    ExprKind::App(Box::new(head), cs.into())
                }
            }
            K::BinExpr => {
                // The operator is whichever token the table knows; its
                // spelling decides the `_op_` name the parser desugared to.
                let op = n
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| self.ops.find(t.text()).is_some());
                let cs: Vec<SyntaxNode> = n.children().collect();
                match (op, cs.as_slice()) {
                    (Some(tok), [l, r]) => {
                        let r_ = tok.text_range();
                        let op_span = Span {
                            start: r_.start().into(),
                            end: r_.end().into(),
                        };
                        let f = self.mk(op_span, ExprKind::Name(op_name(tok.text())));
                        let (l, r) = (self.expr(l), self.expr(r));
                        ExprKind::App(Box::new(f), vec![l, r].into())
                    }
                    _ => ExprKind::Error,
                }
            }
            K::NegExpr => match n.children().next() {
                Some(c) => {
                    let inner = self.expr(&c);
                    match inner.kind {
                        // `checked_neg` rather than `-v`: negating `i64::MIN`
                        // overflows, which panicked in a debug build and
                        // wrapped silently in a release one. Doubling a minus
                        // sign on the `min` literal is the only way there, and
                        // it yields `min` again.
                        ExprKind::Lit(Lit::Int(v)) => match v.checked_neg() {
                            Some(n) => ExprKind::Lit(Lit::Int(n)),
                            None => ExprKind::Lit(Lit::Int(i64::MIN)),
                        },
                        // A literal too large for `i64`; the parser accepts its
                        // digits only here, under a `-`.
                        ExprKind::Error if c.text() == I64_MIN_MAGNITUDE => {
                            ExprKind::Lit(Lit::Int(i64::MIN))
                        }
                        ExprKind::Lit(Lit::Float(s)) => {
                            ExprKind::Lit(Lit::Float(match s.strip_prefix('-') {
                                Some(pos) => pos.to_string(),
                                None => format!("-{s}"),
                            }))
                        }
                        _ => {
                            let f = self.mk(sp, ExprKind::Name("negate".to_string()));
                            ExprKind::App(Box::new(f), vec![inner].into())
                        }
                    }
                }
                None => ExprKind::Error,
            },
            K::ParenExpr => match n.children().next() {
                Some(c) => return self.expr(&c),
                None => ExprKind::Error,
            },
            K::Lambda => match (token(n, K::Ident), n.children().next()) {
                (Some(p), Some(b)) => {
                    ExprKind::Lambda(p.text().to_string(), Box::new(self.expr(&b)))
                }
                _ => ExprKind::Error,
            },
            K::RecordExpr => ExprKind::Record(
                n.children()
                    .filter(|c| c.kind() == K::RecordField)
                    .filter_map(|f| {
                        // `name = expr`, or the shorthand `.name`, which
                        // stands for `name = .name`.
                        if let Some(t) = token(&f, K::Field) {
                            let name = t.text()[1..].to_string();
                            let r = t.text_range();
                            let sp = Span {
                                start: r.start().into(),
                                end: r.end().into(),
                            };
                            let v = self.mk(sp, ExprKind::Field(Side::Single, name.clone()));
                            return Some((name, v));
                        }
                        let name = token(&f, K::Ident)?.text().to_string();
                        let v = f.children().next()?;
                        Some((name, self.expr(&v)))
                    })
                    .collect(),
            ),
            K::ListExpr => ExprKind::List(n.children().map(|c| self.expr(&c)).collect()),
            K::SqlExpr => match token(n, K::String) {
                Some(t) => ExprKind::Sql(unescape(t.text())),
                None => ExprKind::Error,
            },
            _ => ExprKind::Error,
        };
        self.mk(sp, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expr_stays_small() {
        // Every AST node carries `kind`, so its size is paid by every node,
        // not just applications. This pins the storage choice for `App`'s
        // arguments: inline arrays (smallvec) would triple every node.
        eprintln!("size_of::<Expr>() = {}", std::mem::size_of::<Expr>());
        assert!(std::mem::size_of::<Expr>() <= 64);
    }

    fn body(src: &str) -> ExprKind {
        let (m, errs) = lower_source(src);
        assert!(errs.is_empty(), "{errs:?}");
        m.defs.into_iter().next().unwrap().body.kind
    }

    fn name(e: &Expr) -> &str {
        match &e.kind {
            ExprKind::Name(n) => n,
            k => panic!("expected name, got {k:?}"),
        }
    }

    #[test]
    fn comparison_desugars_to_operator_app() {
        let ExprKind::App(f, args) = body("adult = .age >= 18") else {
            panic!()
        };
        assert_eq!(name(&f), "_>=_");
        assert_eq!(args[0].kind, ExprKind::Field(Side::Single, "age".into()));
        assert_eq!(args[1].kind, ExprKind::Lit(Lit::Int(18)));
    }

    #[test]
    fn pipeline_and_join_fields() {
        let ExprKind::App(f, args) = body("r = orders & inner users (.<user_id == .>id)") else {
            panic!()
        };
        assert_eq!(name(&f), "_&_");
        let ExprKind::App(g, gargs) = &args[1].kind else {
            panic!()
        };
        assert_eq!(name(g), "inner");
        let ExprKind::App(eq, sides) = &gargs[1].kind else {
            panic!()
        };
        assert_eq!(name(eq), "_==_");
        assert_eq!(sides[0].kind, ExprKind::Field(Side::Left, "user_id".into()));
        assert_eq!(sides[1].kind, ExprKind::Field(Side::Right, "id".into()));
    }

    #[test]
    fn diamond_is_right_associative() {
        let ExprKind::App(f, args) = body("s = .a <> .b <> .c") else {
            panic!()
        };
        assert_eq!(name(&f), "_<>_");
        assert_eq!(args[0].kind, ExprKind::Field(Side::Single, "a".into()));
        let ExprKind::App(g, _) = &args[1].kind else {
            panic!("expected .b <> .c on the right")
        };
        assert_eq!(name(g), "_<>_");
        // `<` and `<>` stay distinct.
        let ExprKind::App(lt, _) = body("p = .a < .b") else {
            panic!()
        };
        assert_eq!(name(&lt), "_<_");
    }

    #[test]
    fn imports_types_literals() {
        let (m, errs) = lower_source(
            "import \"schema.cagara\" as s\nu : query { id = int | r } = s.users\nt = true\nn = -3\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(m.imports[0].path, "schema.cagara");
        assert_eq!(m.imports[0].alias.as_deref(), Some("s"));
        let Some(TypeExpr::App { head, args, .. }) = &m.defs[0].ty else {
            panic!()
        };
        assert_eq!(head, "query");
        let TypeExpr::Record { fields, tail, .. } = &args[0] else {
            panic!()
        };
        assert_eq!(fields[0].0, "id");
        assert_eq!(tail.as_deref(), Some("r"));
        assert!(matches!(&m.defs[0].body.kind, ExprKind::Proj(_, f) if f == "users"));
        assert_eq!(m.defs[1].body.kind, ExprKind::Lit(Lit::Bool(true)));
        assert_eq!(m.defs[2].body.kind, ExprKind::Lit(Lit::Int(-3)));
    }

    #[test]
    fn records_lambdas_sql() {
        let (m, errs) = lower_source(
            "p = { id = .id, label = upper .name }\nf = x => x\nup : expr r string -> expr r string = sql \"UPPER($1)\"\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        let ExprKind::Record(fs) = &m.defs[0].body.kind else {
            panic!()
        };
        assert_eq!(
            fs.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            ["id", "label"]
        );
        assert!(matches!(&m.defs[1].body.kind, ExprKind::Lambda(p, _) if p == "x"));
        assert_eq!(m.defs[2].body.kind, ExprKind::Sql("UPPER($1)".into()));
    }

    #[test]
    fn negative_literals() {
        assert_eq!(
            body("x = -9223372036854775808"),
            ExprKind::Lit(Lit::Int(i64::MIN))
        );
        assert_eq!(body("x = - -1.5"), ExprKind::Lit(Lit::Float("1.5".into())));
        assert_eq!(body("x = -1.5"), ExprKind::Lit(Lit::Float("-1.5".into())));
    }

    /// Negating `i64::MIN` overflows. It used to panic in a debug build and
    /// wrap silently in a release one, so a doubled minus sign on the `min`
    /// literal aborted the compiler.
    #[test]
    fn negating_the_min_literal_does_not_overflow() {
        // `-(-2^63)` is `+2^63`, which is not an `i64`. Clamping back to
        // `min` keeps the lowering total and the value representable.
        assert_eq!(
            body("x = - -9223372036854775808"),
            ExprKind::Lit(Lit::Int(i64::MIN))
        );
    }

    /// A backslash that is not part of a defined escape has to survive: it is
    /// how a Windows path, a regex, or a SQL string template is written.
    /// Dropping it silently changed the value (`"a\zb"` became `"azb"`), and
    /// a trailing backslash disappeared completely.
    #[test]
    fn undefined_escapes_keep_their_backslash() {
        fn s(src: &str) -> String {
            match body(src) {
                ExprKind::Lit(Lit::Str(v)) => v,
                o => panic!("not a string: {o:?}"),
            }
        }
        // The defined escapes still resolve.
        assert_eq!(s(r#"x = "a\nb""#), "a\nb");
        assert_eq!(s(r#"x = "a\tb""#), "a\tb");
        assert_eq!(s(r#"x = "a\\b""#), r"a\b");
        assert_eq!(s(r#"x = "a\"b""#), "a\"b");
        // An undefined escape keeps both characters instead of losing one.
        assert_eq!(s(r#"x = "a\zb""#), r"a\zb");
        // `\t` is a defined escape, so a single backslash before `t` is a tab;
        // `\\t` is a literal backslash followed by `t`.
        assert_eq!(s(r#"x = "C:\tmp""#), "C:\tmp");
        assert_eq!(s(r#"x = "C:\\tmp""#), r"C:\tmp");
        // The text `"tail\"` is not a trailing backslash, it is an escaped
        // quote that leaves the literal open — the lexer reports that. A
        // backslash at the end needs the escape spelling.
        let (_, errs) = lower_source("x = \"tail\\\"\n");
        assert!(!errs.is_empty(), "an escaped quote must not close a string");
        assert_eq!(s(r#"x = "a\\""#), r"a\");
        // The same unescaping serves `sql` templates, so a literal backslash
        // reaches the backend as one rather than vanishing.
        let (m, _) =
            lower_source("f : expr r string -> expr r string = sql \"REPLACE($1, '\\')\"\n");
        assert_eq!(
            m.defs[0].body.kind,
            ExprKind::Sql(r"REPLACE($1, '\')".into())
        );
    }
}
