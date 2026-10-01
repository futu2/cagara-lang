//! Runtime values of compile-time evaluation. A query definition evaluates to
//! `Value::Query(Rel)`; column expressions evaluate to `Value::Expr`.

use crate::ir::{Bound, Expr, Frame, JoinKind, Lit, Rel, SetKind};
use cagara_syntax::ast::{self, Span};
use std::rc::Rc;

/// The only operations implemented in Rust. Everything user-facing is a
/// prelude definition built from these and from `sql "..."` templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Table,
    Where,
    Select,
    Update,
    AggStage,
    Order,
    Limit,
    Offset,
    Distinct,
    In,
    Join(JoinKind),
    Set(SetKind),
    Group,
    Asc,
    Desc,
    Rows,
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRow,
    Preceding,
    Following,
}

pub const PRIMS: &[(&str, Prim)] = &[
    ("__table", Prim::Table),
    ("__where", Prim::Where),
    ("__select", Prim::Select),
    ("__update", Prim::Update),
    ("__agg", Prim::AggStage),
    ("__order", Prim::Order),
    ("__limit", Prim::Limit),
    ("__offset", Prim::Offset),
    ("__distinct", Prim::Distinct),
    ("__in", Prim::In),
    ("__innerJoin", Prim::Join(JoinKind::Inner)),
    ("__leftJoin", Prim::Join(JoinKind::Left)),
    ("__rightJoin", Prim::Join(JoinKind::Right)),
    ("__fullJoin", Prim::Join(JoinKind::Full)),
    ("__semiJoin", Prim::Join(JoinKind::Semi)),
    ("__antiJoin", Prim::Join(JoinKind::Anti)),
    ("__union", Prim::Set(SetKind::Union)),
    ("__intersect", Prim::Set(SetKind::Intersect)),
    ("__except", Prim::Set(SetKind::Except)),
    ("__group", Prim::Group),
    ("__asc", Prim::Asc),
    ("__desc", Prim::Desc),
    ("__rows", Prim::Rows),
    ("__unboundedPreceding", Prim::UnboundedPreceding),
    ("__unboundedFollowing", Prim::UnboundedFollowing),
    ("__currentRow", Prim::CurrentRow),
    ("__preceding", Prim::Preceding),
    ("__following", Prim::Following),
];

impl Prim {
    pub fn arity(self) -> usize {
        use Prim::*;
        match self {
            UnboundedPreceding | UnboundedFollowing | CurrentRow => 0,
            Group | Asc | Desc | Preceding | Following => 1,
            Where | Select | Update | AggStage | Order | Limit | Offset | Table | Rows => 2,
            Distinct => 1,
            In => 2,
            Join(_) => 3,
            Set(_) => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TplKind {
    Scalar,
    Agg,
    /// First argument is the window spec record.
    Win,
}

#[derive(Debug)]
pub struct Template {
    pub sql: String,
    pub kind: TplKind,
    pub arity: usize,
}

/// The definition being evaluated and the candidates filling its overload
/// holes (see `check::Choice`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Inst {
    pub def: (usize, usize),
    pub holes: Vec<(usize, usize)>,
}

#[derive(Debug)]
pub struct Closure {
    pub param: String,
    pub body: ast::Expr,
    pub env: Env,
    pub module: usize,
    pub inst: Rc<Inst>,
}

#[derive(Debug, Clone)]
pub enum Value {
    Lit(Lit),
    Record(Vec<(String, Value)>),
    List(Vec<Value>),
    Closure(Rc<Closure>),
    Prim(Prim, Vec<Value>),
    Tpl(Rc<Template>, Vec<Value>),
    Expr(Expr),
    /// Sort key: expression and ascending flag.
    Dir(Expr, bool),
    Query(Rel),
    Frame(Frame),
    Bound(Bound),
}

impl Value {
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Lit(Lit::Int(_)) => "an int",
            Value::Lit(Lit::Float(_)) => "a float",
            Value::Lit(Lit::Str(_)) => "a string",
            Value::Lit(Lit::Bool(_)) => "a bool",
            Value::Record(_) => "a record",
            Value::List(_) => "a list",
            Value::Closure(_) | Value::Prim(..) | Value::Tpl(..) => "a function",
            Value::Expr(_) => "a column expression",
            Value::Dir(..) => "a sort key",
            Value::Query(_) => "a query",
            Value::Frame(_) => "a window frame",
            Value::Bound(_) => "a frame bound",
        }
    }
}

/// Persistent lexical environment for lambda parameters.
#[derive(Debug, Clone, Default)]
pub struct Env(Option<Rc<(String, Value, Env)>>);

impl Env {
    pub fn get(&self, name: &str) -> Option<&Value> {
        let mut cur = self;
        while let Some(node) = &cur.0 {
            if node.0 == name {
                return Some(&node.1);
            }
            cur = &node.2;
        }
        None
    }

    pub fn bind(&self, name: String, v: Value) -> Env {
        Env(Some(Rc::new((name, v, self.clone()))))
    }
}

#[derive(Debug, Clone)]
pub struct EvalError {
    pub module: usize,
    pub span: Option<Span>,
    pub message: String,
}

pub type EResult<T> = Result<T, EvalError>;

pub fn err<T>(message: impl Into<String>) -> EResult<T> {
    Err(EvalError {
        module: 0,
        span: None,
        message: message.into(),
    })
}
