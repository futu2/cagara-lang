//! Relational IR produced by compile-time evaluation and consumed by the SQL
//! backend. Expression phases (row / aggregate / window) are structural: an
//! aggregate or window node may only contain row-phase expressions, which is
//! what enforces the one-level nesting rule for `agg` and `win`.

use crate::rules;
pub use cagara_syntax::ast::{Side, Span};

/// Source location of a query stage in user code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Loc {
    pub module: usize,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Lit {
    Int(i64),
    Float(String),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bound {
    UnboundedPreceding,
    Preceding(i64),
    CurrentRow,
    Following(i64),
    UnboundedFollowing,
}

impl Bound {
    /// Position on the frame axis, used to reject impossible frames.
    pub fn key(self) -> i128 {
        match self {
            Bound::UnboundedPreceding => i128::MIN,
            Bound::Preceding(n) => -(n as i128),
            Bound::CurrentRow => 0,
            Bound::Following(n) => n as i128,
            Bound::UnboundedFollowing => i128::MAX,
        }
    }
}

/// A `ROWS BETWEEN start AND end` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Frame {
    pub start: Bound,
    pub end: Bound,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WinSpec {
    pub partition: Vec<Expr>,
    pub order: Vec<(Expr, bool)>,
    /// `None` omits the frame clause (required for ranking functions).
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    Col(Side, String),
    Lit(Lit),
    /// Scalar SQL template with `$n` placeholders.
    Tpl(String, Vec<Expr>),
    /// Membership test against a compile-time list of expressions.
    In(Box<Expr>, Vec<Expr>, bool),
    /// Aggregate SQL template; arguments must be row-phase.
    Agg(String, Vec<Expr>),
    /// Grouping key (`group e`).
    Group(Box<Expr>),
    /// Window function template plus its OVER specification.
    Win(String, Vec<Expr>, Box<WinSpec>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Const,
    Row,
    Agg,
    Win,
}

impl Expr {
    pub fn phase(&self) -> Result<Phase, String> {
        match self {
            Expr::Col(..) => Ok(Phase::Row),
            Expr::Lit(_) => Ok(Phase::Const),
            Expr::Tpl(_, args) => args
                .iter()
                .try_fold(Phase::Const, |acc, a| rules::mix(acc, a.phase()?)),
            Expr::In(value, list, _) => list
                .iter()
                .chain(std::iter::once(value.as_ref()))
                .try_fold(Phase::Const, |acc, a| rules::mix(acc, a.phase()?)),
            Expr::Agg(_, args) => {
                args.iter()
                    .try_for_each(|a| row_only(a, "an aggregate argument"))?;
                Ok(Phase::Agg)
            }
            Expr::Group(k) => {
                row_only(k, "a group key")?;
                Ok(Phase::Agg)
            }
            Expr::Win(..) => {
                self.children()
                    .into_iter()
                    .try_for_each(|a| row_only(a, "a window argument"))?;
                Ok(Phase::Win)
            }
        }
    }

    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Col(..) | Expr::Lit(_) => vec![],
            Expr::Tpl(_, a) | Expr::Agg(_, a) => a.iter().collect(),
            Expr::In(value, list, _) => {
                list.iter().chain(std::iter::once(value.as_ref())).collect()
            }
            Expr::Group(k) => vec![k.as_ref()],
            Expr::Win(_, a, s) => a
                .iter()
                .chain(&s.partition)
                .chain(s.order.iter().map(|(e, _)| e))
                .collect(),
        }
    }

    pub fn any(&self, p: &dyn Fn(&Expr) -> bool) -> bool {
        p(self) || self.children().into_iter().any(|c| c.any(p))
    }

    pub fn columns(&self) -> Vec<(Side, String)> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut Vec<(Side, String)>) {
        if let Expr::Col(s, n) = self {
            out.push((*s, n.clone()));
        }
        for c in self.children() {
            c.collect_columns(out);
        }
    }
}

fn row_only(e: &Expr, what: &str) -> Result<(), String> {
    rules::nested(what, e.phase()?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Semi,
    Anti,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetKind {
    Union,
    UnionAll,
    Intersect,
    Except,
}

/// Relational structure of one query stage.
///
/// `Eq`/`Hash` are structural, including the [`Loc`] of a [`Rel::At`], and the
/// SQL lowerer uses them as the identity of a shareable sub-pipeline. They
/// must stay consistent with each other, which a derive guarantees and a
/// hand-written pair would not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Rel {
    Table {
        schema: String,
        name: String,
        columns: Option<Vec<String>>,
    },
    Where(Box<Rel>, Expr),
    Select(Box<Rel>, Vec<(String, Expr)>),
    /// `update {..}`: a projection merged over the input row. A listed name
    /// that the input already has keeps its position and takes the new
    /// expression; a name the input does not have is appended at the end.
    Update(Box<Rel>, Vec<(String, Expr)>),
    /// `omit "k"`: the input row without column `k`. In the checker this is
    /// nothing but the row equation `input ~ { k : t | output }`.
    Omit(Box<Rel>, String),
    /// `prefix "s"`: every column name gains `s` in front. The checker types
    /// this as the row term `keyMap (prefix s) input`; the affix is carried
    /// here so the lowerer computes the same names the checker did.
    Prefix(Box<Rel>, String),
    /// `suffix "s"`: every column name gains `s` at the end.
    Suffix(Box<Rel>, String),
    Agg(Box<Rel>, Vec<(String, Expr)>),
    Order(Box<Rel>, Vec<(Expr, bool)>),
    Limit(Box<Rel>, i64),
    Offset(Box<Rel>, i64),
    Distinct(Box<Rel>),
    /// `left & innerJoin right on`; output columns are left-wins on collision.
    Join {
        kind: JoinKind,
        left: Box<Rel>,
        right: Box<Rel>,
        on: Expr,
    },
    /// A set operation over two relations with the same output row.
    Set {
        kind: SetKind,
        left: Box<Rel>,
        right: Box<Rel>,
    },
    /// The stage written at `Loc` (transparent for schema and lowering).
    At(Loc, Box<Rel>),
}

impl Rel {
    pub fn children(&self) -> Vec<&Rel> {
        match self {
            Rel::Table { .. } => vec![],
            Rel::Where(r, _)
            | Rel::Select(r, _)
            | Rel::Update(r, _)
            | Rel::Omit(r, _)
            | Rel::Prefix(r, _)
            | Rel::Suffix(r, _)
            | Rel::Agg(r, _)
            | Rel::Order(r, _)
            | Rel::Limit(r, _)
            | Rel::Offset(r, _)
            | Rel::Distinct(r)
            | Rel::At(_, r) => vec![r],
            Rel::Join { left, right, .. } => vec![left, right],
            Rel::Set { left, right, .. } => vec![left, right],
        }
    }

    /// The relation without location wrappers at its root.
    pub fn bare(&self) -> &Rel {
        match self {
            Rel::At(_, r) => r.bare(),
            r => r,
        }
    }
}
