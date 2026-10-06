//! Runtime values of compile-time evaluation. A query definition evaluates to
//! `Value::Query(CoreTerm)` and a column expression to `Value::Expr(CoreTerm)`:
//! the evaluator's by-product is the explicit core representation, which is
//! erased to `Rel` only at the boundary where the SQL backend and the schema
//! validator need it (see `eval::root_queries_checked`).

use crate::core_term::CoreTerm;
use crate::ir::{Bound, Frame, JoinKind, Lit, SetKind};
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
    Omit,
    MapValue,
    Merge,
    Prefix,
    Suffix,
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
    ("__omit", Prim::Omit),
    ("__mapValue", Prim::MapValue),
    ("__merge", Prim::Merge),
    ("__prefix", Prim::Prefix),
    ("__suffix", Prim::Suffix),
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
    ("__unionAll", Prim::Set(SetKind::UnionAll)),
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
    /// How many arguments this primitive takes before it saturates.
    ///
    /// Kept in step with [`Prim::classify`] by hand: `arity` says how many
    /// arguments there are, `classify` says what they build, and the
    /// classification's `Query`/`Expr`/`Key`/`Frame`/`Bound` arms are grouped
    /// from the same shape. A variant whose arity and kind disagree would not
    /// be a type error, so change the two together.
    pub fn arity(self) -> usize {
        use Prim::*;
        match self {
            UnboundedPreceding | UnboundedFollowing | CurrentRow => 0,
            Group | Asc | Desc | Preceding | Following => 1,
            Where | Select | Update | Omit | AggStage | Order | Limit | Offset | Table | Rows
            | Prefix | Suffix | Merge | MapValue => 2,
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
    /// A scalar/aggregate/window expression, in the explicit core form.
    ///
    /// Boxed, like the two below: `CoreTerm` is a recursive tree of `Vec`s and
    /// `String`s and is 128 bytes, so holding one inline made `Value` 136 bytes
    /// instead of 80 — a 70% growth in the enum that every `eval`/`apply`/
    /// `inst_value` frame stores several of. With `MAX_DEPTH = 256` nested
    /// calls it has to fit a 2 MiB thread (the language server's, and the one
    /// `cargo test` uses), and at 136 bytes it did not:
    /// `mutual_application_is_reported_too` overflowed the stack. The box
    /// keeps the payload off the enum, so `Value` is the baseline's size again.
    Expr(Box<CoreTerm>),
    /// Sort key: expression and ascending flag.
    Dir(Box<CoreTerm>, bool),
    /// A query, in the explicit core form (erased to `Rel` at the boundary).
    Query(Box<CoreTerm>),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `Value`'s size is a stack budget, not a style question.
    ///
    /// `MAX_DEPTH` allows 256 nested `eval`/`apply` calls, and every such
    /// frame stores several `Value`s (the environment binding, the return
    /// slot, `apply`'s own locals). The language server runs the evaluator on
    /// threads as small as 2 MiB (`eval::MAX_DEF_NESTING` records the same
    /// budget), and `cargo test` uses 2 MiB threads too — so an inline
    /// recursive payload inflates every frame and eventually overflows the
    /// stack before `MAX_DEPTH` can fire.
    ///
    /// That is not hypothetical: holding `CoreTerm` (128 bytes, a recursive
    /// tree of `Vec`s and `String`s) inline in `Query`/`Expr`/`Dir` made
    /// `Value` 136 bytes and `eval::tests::mutual_application_is_reported_too`
    /// overflowed the stack instead of reporting the recursion it exists to
    /// report. The payloads are `Box`ed; this pins the result.
    ///
    /// The bound is generous (the payloads are boxed, so the enum is a tag plus
    /// the largest inline variant) because the point is to catch an inline
    /// `CoreTerm` coming back, not to freeze the exact size.
    #[test]
    fn value_stays_small_enough_for_its_stack_budget() {
        let v = std::mem::size_of::<Value>();
        assert!(
            v <= 64,
            "`Value` is {v} bytes; the 256-deep evaluator recursion has to fit a 2 MiB \
             thread, so the `CoreTerm` payloads in Query/Expr/Dir must stay boxed"
        );
        // A `CoreTerm` kept inline would show up as roughly its own size here.
        assert!(
            v < std::mem::size_of::<CoreTerm>(),
            "`Value` ({v}) must not carry a `CoreTerm` inline ({} bytes)",
            std::mem::size_of::<CoreTerm>()
        );
    }
}
