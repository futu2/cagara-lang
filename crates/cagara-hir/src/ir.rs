//! Relational IR produced by compile-time evaluation and consumed by the SQL
//! backend. Expression phases (row / aggregate / window) are structural: an
//! aggregate or window node may only contain row-phase expressions, which is
//! what enforces the one-level nesting rule for `agg` and `win`.

pub use cagara_syntax::ast::Side;

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Int(i64),
    Float(String),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub start: Bound,
    pub end: Bound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WinSpec {
    pub partition: Vec<Expr>,
    pub order: Vec<(Expr, bool)>,
    /// `None` omits the frame clause (required for ranking functions).
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Col(Side, String),
    Lit(Lit),
    /// Scalar SQL template with `$n` placeholders.
    Tpl(String, Vec<Expr>),
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
            Expr::Tpl(_, args) => args.iter().try_fold(Phase::Const, |acc, a| join(acc, a.phase()?)),
            Expr::Agg(_, args) => {
                args.iter().try_for_each(|a| row_only(a, "an aggregate argument"))?;
                Ok(Phase::Agg)
            }
            Expr::Group(k) => {
                row_only(k, "a group key")?;
                Ok(Phase::Agg)
            }
            Expr::Win(..) => {
                self.children().into_iter().try_for_each(|a| row_only(a, "a window argument"))?;
                Ok(Phase::Win)
            }
        }
    }

    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Col(..) | Expr::Lit(_) => vec![],
            Expr::Tpl(_, a) | Expr::Agg(_, a) => a.iter().collect(),
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
    match e.phase()? {
        Phase::Const | Phase::Row => Ok(()),
        Phase::Agg => Err(format!(
            "{what} contains an aggregate; aggregates cannot nest (aggregate in an earlier `agg` stage)"
        )),
        Phase::Win => Err(format!(
            "{what} contains a window function; windows cannot nest (compute it in an earlier `select` stage)"
        )),
    }
}

fn join(a: Phase, b: Phase) -> Result<Phase, String> {
    use Phase::*;
    match (a, b) {
        (Const, p) | (p, Const) => Ok(p),
        (Row, Row) => Ok(Row),
        (Agg, Agg) => Ok(Agg),
        (Win, Win) | (Row, Win) | (Win, Row) => Ok(Win),
        (Row, Agg) | (Agg, Row) => {
            Err("mixes an aggregate with an ungrouped column; wrap the column in `group`".into())
        }
        (Agg, Win) | (Win, Agg) => Err(
            "mixes an aggregate with a window function; use `agg` first, then `select` the window".into(),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// Closed, compiler-known key mappers (the basis of pick / omit / rename).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyMapper {
    Only(Vec<String>),
    Drop(Vec<String>),
    Replace(Vec<(String, String)>),
    Prefix(String),
    Suffix(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rel {
    Table { schema: String, name: String, columns: Option<Vec<String>> },
    Where(Box<Rel>, Expr),
    Select(Box<Rel>, Vec<(String, Expr)>),
    Agg(Box<Rel>, Vec<(String, Expr)>),
    Order(Box<Rel>, Vec<(Expr, bool)>),
    Limit(Box<Rel>, i64),
    Offset(Box<Rel>, i64),
    KeyMap(Box<Rel>, KeyMapper),
    /// `left & inner right on`; output columns are left-wins on collision.
    Join { kind: JoinKind, left: Box<Rel>, right: Box<Rel>, on: Expr },
}
