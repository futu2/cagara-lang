//! Owned AST lowered from the lossless rowan tree. Operators are desugared
//! into applications of their operator names (`a + b` => `_+_ a b`), so the
//! core has no built-in knowledge of arithmetic or pipelines.

use crate::{parse, ParseError, SyntaxKind as K, SyntaxNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Module {
    pub imports: Vec<Import>,
    pub defs: Vec<Def>,
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
    App { head: String, args: Vec<TypeExpr>, span: Span },
    Record { fields: Vec<(String, TypeExpr)>, tail: Option<String>, span: Span },
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
    App(Box<Expr>, Vec<Expr>),
    Lambda(String, Box<Expr>),
    Record(Vec<(String, Expr)>),
    List(Vec<Expr>),
    /// `sql "template"` with `$1`, `$2` placeholders
    Sql(String),
    Error,
}

/// Parse and lower a source file. Parse errors are returned alongside a
/// best-effort module (erroneous parts become `ExprKind::Error`).
pub fn lower_source(src: &str) -> (Module, Vec<ParseError>) {
    let parse = parse(src);
    let mut l = Lower { next: 0 };
    let module = l.module(&parse.syntax());
    (module, parse.errors)
}

struct Lower {
    next: ExprId,
}

fn span(n: &SyntaxNode) -> Span {
    let r = n.text_range();
    Span { start: r.start().into(), end: r.end().into() }
}

fn token(n: &SyntaxNode, k: K) -> Option<crate::SyntaxToken> {
    n.children_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == k)
}

fn unescape(s: &str) -> String {
    let inner = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Lower {
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
                _ => {}
            }
        }
        m
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
        Some(Def { name, ty, body, span: span(n) })
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
                TypeExpr::Record { fields, tail, span: span(n) }
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
                let t = n.children_with_tokens().filter_map(|e| e.into_token()).find(|t| {
                    matches!(t.kind(), K::Int | K::Float | K::String)
                });
                match t {
                    Some(t) if t.kind() == K::Int => match t.text().parse() {
                        Ok(v) => ExprKind::Lit(Lit::Int(v)),
                        Err(_) => ExprKind::Error,
                    },
                    Some(t) if t.kind() == K::Float => ExprKind::Lit(Lit::Float(t.text().to_string())),
                    Some(t) => ExprKind::Lit(Lit::Str(unescape(t.text()))),
                    None => ExprKind::Error,
                }
            }
            K::FieldExpr => {
                let t = n.children_with_tokens().filter_map(|e| e.into_token()).find(|t| {
                    matches!(t.kind(), K::Field | K::LeftField | K::RightField)
                });
                match t {
                    Some(t) if t.kind() == K::LeftField => ExprKind::Field(Side::Left, t.text()[2..].to_string()),
                    Some(t) if t.kind() == K::RightField => ExprKind::Field(Side::Right, t.text()[2..].to_string()),
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
                    ExprKind::App(Box::new(head), cs)
                }
            }
            K::BinExpr => {
                let op = n
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find_map(|t| t.kind().infix().map(|(_, _, name)| (name, t)));
                let cs: Vec<SyntaxNode> = n.children().collect();
                match (op, cs.as_slice()) {
                    (Some((name, tok)), [l, r]) => {
                        let r_ = tok.text_range();
                        let op_span = Span { start: r_.start().into(), end: r_.end().into() };
                        let f = self.mk(op_span, ExprKind::Name(name.to_string()));
                        let (l, r) = (self.expr(l), self.expr(r));
                        ExprKind::App(Box::new(f), vec![l, r])
                    }
                    _ => ExprKind::Error,
                }
            }
            K::NegExpr => match n.children().next() {
                Some(c) => {
                    let inner = self.expr(&c);
                    match inner.kind {
                        ExprKind::Lit(Lit::Int(v)) => ExprKind::Lit(Lit::Int(-v)),
                        ExprKind::Lit(Lit::Float(s)) => ExprKind::Lit(Lit::Float(format!("-{s}"))),
                        _ => {
                            let f = self.mk(sp, ExprKind::Name("negate".to_string()));
                            ExprKind::App(Box::new(f), vec![inner])
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
                (Some(p), Some(b)) => ExprKind::Lambda(p.text().to_string(), Box::new(self.expr(&b))),
                _ => ExprKind::Error,
            },
            K::RecordExpr => ExprKind::Record(
                n.children()
                    .filter(|c| c.kind() == K::RecordField)
                    .filter_map(|f| {
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
        let ExprKind::App(f, args) = body("adult = .age >= 18") else { panic!() };
        assert_eq!(name(&f), "_>=_");
        assert_eq!(args[0].kind, ExprKind::Field(Side::Single, "age".into()));
        assert_eq!(args[1].kind, ExprKind::Lit(Lit::Int(18)));
    }

    #[test]
    fn pipeline_and_join_fields() {
        let ExprKind::App(f, args) = body("r = orders & inner users (.<user_id == .>id)") else { panic!() };
        assert_eq!(name(&f), "_&_");
        let ExprKind::App(g, gargs) = &args[1].kind else { panic!() };
        assert_eq!(name(g), "inner");
        let ExprKind::App(eq, sides) = &gargs[1].kind else { panic!() };
        assert_eq!(name(eq), "_==_");
        assert_eq!(sides[0].kind, ExprKind::Field(Side::Left, "user_id".into()));
        assert_eq!(sides[1].kind, ExprKind::Field(Side::Right, "id".into()));
    }

    #[test]
    fn diamond_is_right_associative() {
        let ExprKind::App(f, args) = body("s = .a <> .b <> .c") else { panic!() };
        assert_eq!(name(&f), "_<>_");
        assert_eq!(args[0].kind, ExprKind::Field(Side::Single, "a".into()));
        let ExprKind::App(g, _) = &args[1].kind else { panic!("expected .b <> .c on the right") };
        assert_eq!(name(g), "_<>_");
        // `<` and `<>` stay distinct.
        let ExprKind::App(lt, _) = body("p = .a < .b") else { panic!() };
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
        let Some(TypeExpr::App { head, args, .. }) = &m.defs[0].ty else { panic!() };
        assert_eq!(head, "query");
        let TypeExpr::Record { fields, tail, .. } = &args[0] else { panic!() };
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
        let ExprKind::Record(fs) = &m.defs[0].body.kind else { panic!() };
        assert_eq!(fs.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["id", "label"]);
        assert!(matches!(&m.defs[1].body.kind, ExprKind::Lambda(p, _) if p == "x"));
        assert_eq!(m.defs[2].body.kind, ExprKind::Sql("UPPER($1)".into()));
    }
}
