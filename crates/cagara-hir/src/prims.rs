//! Rust implementations of the `__` primitives and of `sql` template
//! saturation. Phase rules are checked as soon as an expression is built, so
//! errors point at the offending application.
//!
//! Every primitive is *classified* before it is applied: [`Prim::classify`]
//! is an exhaustive `match` that says which kind of thing a primitive builds,
//! and each kind has one place that turns arguments into a [`CoreTerm`]. A new
//! `Prim` variant without a classification is a compile error, not a runtime
//! fallthrough — which is the whole point of moving from a name-keyed dispatch
//! table to explicit core constructors.

use crate::core_term::CoreTerm;
use crate::ir::{Bound, Frame, Lit};
use crate::value::{err, EResult, Prim, Template, TplKind, Value};

pub fn lift(v: Value) -> EResult<CoreTerm> {
    core_expr(v)
}

fn lift_all(vs: Vec<Value>) -> EResult<Vec<CoreTerm>> {
    vs.into_iter().map(lift).collect()
}

fn checked(t: CoreTerm) -> EResult<Value> {
    core_phase(&t)?;
    Ok(Value::Expr(Box::new(t)))
}

pub fn build_tpl(t: &Template, args: Vec<Value>) -> EResult<Value> {
    let sql = t.sql.clone();
    match t.kind {
        // A scalar template is top-level like any other expression: it may
        // wrap a window or an aggregate, but it cannot be mixed with one. If
        // its arguments already are one, the whole template is that phase,
        // which keeps `inc : agg (expr r int) -> expr r int = sql "$1 + 1"`
        // an aggregate rather than an aggregate inside a scalar.
        TplKind::Scalar => checked(CoreTerm::tpl(sql, lift_all(args)?)),
        TplKind::Agg => checked(core(CoreTerm::agg_expr(sql, lift_all(args)?))?),
        TplKind::Win => {
            let mut it = args.into_iter();
            let spec = match it.next() {
                Some(s) => core_spec(s)?,
                None => return err("window function without a spec argument"),
            };
            checked(core(CoreTerm::win(sql, lift_all(it.collect())?, spec))?)
        }
    }
}

fn string(v: Value) -> EResult<String> {
    match v {
        Value::Lit(Lit::Str(s)) => Ok(s),
        o => err(format!("expected a string, found {}", o.kind())),
    }
}

fn list(v: Value) -> EResult<Vec<Value>> {
    match v {
        Value::List(xs) => Ok(xs),
        o => err(format!("expected a list, found {}", o.kind())),
    }
}

fn expressions(v: Value) -> EResult<Vec<CoreTerm>> {
    list(v)?.into_iter().map(lift).collect()
}

fn count(v: Value, what: &str) -> EResult<i64> {
    match v {
        Value::Lit(Lit::Int(n)) if n >= 0 => Ok(n),
        Value::Lit(Lit::Int(n)) => err(format!("`{what}` needs a non-negative count, got {n}")),
        o => err(format!("`{what}` expects an int, found {}", o.kind())),
    }
}

pub fn sort_key(v: Value) -> EResult<(CoreTerm, bool)> {
    core_key(v)
}

fn bound(v: Value) -> EResult<Bound> {
    match v {
        Value::Bound(b) => Ok(b),
        o => err(format!("expected a frame bound, found {}", o.kind())),
    }
}


// ── the classification ─────────────────────────────────────────────────────
//
// Every primitive says, statically, which *kind* of thing it builds. The kinds
// are the ones the evaluator has always had — a query stage, a sort key, a
// frame bound — but they are now an enum the compiler checks rather than a
// convention in a `match` arm's body. Adding a `Prim` variant without giving
// it a `Kind` does not compile; it used to be a silent fallthrough.

/// What a primitive builds when it is saturated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Relational: the argument is a query, the result is a `CoreTerm` query.
    Query,
    /// Scalar: builds an expression node (`.age + 1`, `in`, `group`).
    Expr,
    /// Sort key: expression plus direction, only meaningful inside `order`
    /// and a window spec.
    Key,
    /// Window frame (`rows a b`).
    Frame,
    /// Window frame bound.
    Bound,
    /// A type-level operation the checker has already reduced; reaching the
    /// evaluator is an error, and stays one.
    TypeLevel,
}

impl Prim {
    /// The kind of thing this primitive builds. Exhaustive by construction.
    pub fn classify(self) -> Kind {
        use Prim::*;
        match self {
            Table | Where | Select | Update | Omit | Prefix | Suffix | AggStage | Order | Limit
            | Offset | Distinct | Join(_) | Set(_) => Kind::Query,
            In | Group => Kind::Expr,
            Asc | Desc => Kind::Key,
            Rows => Kind::Frame,
            UnboundedPreceding | UnboundedFollowing | CurrentRow | Preceding | Following => {
                Kind::Bound
            }
            MapValue | Merge => Kind::TypeLevel,
        }
    }
}

/// Convert a core-layer error into the evaluator's error type. The constructors
/// state the rule; this only transports the message, so a user sees one
/// explanation (`schema` reports the same wording for an untyped tree).
fn core(r: Result<CoreTerm, crate::core::Error>) -> EResult<CoreTerm> {
    match r {
        Ok(t) => Ok(t),
        Err(e) => err(e.message),
    }
}

/// A core query argument. A `Value::Query` that was built by a `Prim` holds a
/// `CoreTerm`; one that came from somewhere else is an error, as before.
fn core_query(v: Value) -> EResult<CoreTerm> {
    match v {
        Value::Query(t) => Ok(*t),
        o => err(format!("expected a query, found {}", o.kind())),
    }
}

/// A core expression argument: the leaf a stage predicate, field or key is.
fn core_expr(v: Value) -> EResult<CoreTerm> {
    match v {
        Value::Expr(t) => Ok(*t),
        Value::Lit(l) => Ok(CoreTerm::lit(l)),
        o => err(format!(
            "expected a column expression or constant, found {}",
            o.kind()
        )),
    }
}

/// A record of fields, each turned into a core expression. The label is kept
/// on the error, as it was.
fn core_fields(v: Value) -> EResult<Vec<(String, CoreTerm)>> {
    match v {
        Value::Record(fs) => fs
            .into_iter()
            .map(|(k, v)| {
                let t = core_expr(v).map_err(|mut e| {
                    e.message = format!("field `{k}`: {}", e.message);
                    e
                })?;
                Ok((k, t))
            })
            .collect(),
        o => err(format!(
            "expected a record of column expressions, found {}",
            o.kind()
        )),
    }
}

/// One sort key: an explicit `CoreTerm::dir`, or a bare expression defaulted to
/// ascending (what `sort_key` has always done).
fn core_key(v: Value) -> EResult<(CoreTerm, bool)> {
    match v {
        Value::Dir(t, asc) => Ok((*t, asc)),
        Value::Expr(t) => Ok((*t, true)),
        Value::Lit(l) => Ok((CoreTerm::lit(l), true)),
        o => err(format!("expected a sort key, found {}", o.kind())),
    }
}

fn core_keys(v: Value) -> EResult<Vec<(CoreTerm, bool)>> {
    list(v)?.into_iter().map(core_key).collect()
}

/// A window spec record → the core spec the `win` constructor takes.
fn core_spec(v: Value) -> EResult<crate::core_term::CoreSpec> {
    let Value::Record(fs) = v else {
        return err(format!(
            "a window function expects {{ {} }}, found {}",
            crate::rules::winspec::names(),
            v.kind()
        ));
    };
    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    for (k, v) in fs {
        match k.as_str() {
            crate::rules::winspec::PARTITION => {
                partition = list(v)?.into_iter().map(core_expr).collect::<EResult<_>>()?
            }
            crate::rules::winspec::ORDER => order = core_keys(v)?,
            crate::rules::winspec::FRAME => match v {
                Value::Frame(f) => frame = Some(f),
                o => {
                    return err(format!(
                        "`{}` must be a frame such as `wholePartition`, found {}",
                        crate::rules::winspec::FRAME,
                        o.kind()
                    ))
                }
            },
            other => {
                return err(format!(
                    "unknown window spec field `{other}`; expected {}",
                    crate::rules::winspec::names()
                ))
            }
        }
    }
    Ok(crate::core_term::CoreSpec::new(partition, order, frame))
}

/// The phase check the old `where` arm did inline: `pred.phase()` had to be
/// `Ok`, and its message was the evaluator's. The core constructor keeps the
/// same wording via `Phase::phase`.
fn core_phase(t: &CoreTerm) -> EResult<()> {
    match t.phase() {
        Ok(_) => Ok(()),
        Err(m) => err(m),
    }
}

/// Apply a saturated primitive. Argument order follows the prelude's
/// curried signatures, with the query argument last so `q & where p` works.
///
/// The classification happens first (`p.classify()`), and each kind has one
/// place that builds its core node. The *shape* of the result — a query, an
/// expression, a key, a frame, a bound — is what `classify` says it is, and
/// nothing here re-discovers it from a name.
pub fn call(p: Prim, args: Vec<Value>) -> EResult<Value> {
    let kind = p.classify();
    let mut a = args.into_iter();
    let mut next = || a.next().expect("primitive called with its full arity");
    match kind {
        Kind::TypeLevel => err(format!(
            "{} is a type-level operation and should be resolved during type checking",
            type_level_name(p)
        )),
        Kind::Query => Ok(Value::Query(Box::new(query_prim(p, &mut next)?))),
        Kind::Expr => Ok(Value::Expr(Box::new(expr_prim(p, &mut next)?))),
        Kind::Key => {
            let (t, asc) = match p {
                Prim::Asc => (core_expr(next())?, true),
                Prim::Desc => (core_expr(next())?, false),
                // `classify` says this kind is only Asc/Desc.
                _ => unreachable!("only `asc`/`desc` build a sort key"),
            };
            Ok(Value::Dir(Box::new(t), asc))
        }
        Kind::Frame => Ok(Value::Frame(frame_prim(&mut next)?)),
        Kind::Bound => Ok(Value::Bound(bound_prim(p, &mut next)?)),
    }
}

/// The name a type-level primitive is reported under. `classify` is total, so
/// this match only has to be exhaustive over the type-level half.
fn type_level_name(p: Prim) -> &'static str {
    match p {
        Prim::MapValue => "mapValue",
        Prim::Merge => "merge",
        _ => unreachable!("not a type-level primitive"),
    }
}

/// Build one relational node. Every arm is a named `CoreTerm` constructor:
/// this is where the *rule* for the stage lives now, not in a `Rel` variant
/// assembled here.
///
/// **Argument order.** `next()` reads the arguments in the order the prelude's
/// curried signature supplies them, which puts the *stage* argument first and
/// the query last (`select : expr r (row s) -> query r -> query s`). Each arm
/// therefore reads its stage argument before the query it applies to — the
/// same order the original `prims::call` used. Since `call` receives a
/// `Vec<Value>` and saturates on `arity()`, a permutation here type-checks and
/// compiles but hands a record to `core_query` at run time, which is why the
/// stage-form tests in `eval/elaboration_tests.rs` pin the order for every
/// primitive.
fn query_prim(p: Prim, next: &mut impl FnMut() -> Value) -> EResult<CoreTerm> {
    use Prim::*;
    match p {
        Table => Ok(CoreTerm::table(string(next())?, string(next())?)),
        Where => {
            let pred = core_expr(next())?;
            core_phase(&pred)?;
            core(CoreTerm::where_(core_query(next())?, pred))
        }
        Select => {
            let fs = core_fields(next())?;
            core(CoreTerm::select(core_query(next())?, fs))
        }
        Update => {
            let fs = core_fields(next())?;
            core(CoreTerm::update(core_query(next())?, fs))
        }
        Omit => {
            let key = string(next())?;
            core(CoreTerm::omit(core_query(next())?, key))
        }
        Prefix => {
            let affix = string(next())?;
            core(CoreTerm::prefix(core_query(next())?, affix))
        }
        Suffix => {
            let affix = string(next())?;
            core(CoreTerm::suffix(core_query(next())?, affix))
        }
        AggStage => {
            let fs = core_fields(next())?;
            core(CoreTerm::agg(core_query(next())?, fs))
        }
        Order => {
            let keys = core_keys(next())?;
            core(CoreTerm::order(core_query(next())?, keys))
        }
        Limit => {
            let n = count(next(), "limit")?;
            core(CoreTerm::limit(core_query(next())?, n))
        }
        Offset => {
            let n = count(next(), "offset")?;
            core(CoreTerm::offset(core_query(next())?, n))
        }
        Distinct => core(CoreTerm::distinct(core_query(next())?)),
        Join(kind) => {
            let right = core_query(next())?;
            let on = core_expr(next())?;
            let left = core_query(next())?;
            core(CoreTerm::join(kind, left, right, on))
        }
        Set(kind) => {
            let left = core_query(next())?;
            let right = core_query(next())?;
            Ok(CoreTerm::set(kind, left, right))
        }
        // `classify` routes these elsewhere.
        In | Group | Asc | Desc | Rows | UnboundedPreceding | UnboundedFollowing | CurrentRow
        | Preceding | Following | MapValue | Merge => {
            unreachable!("`{}` is not a query stage", prim_name(p))
        }
    }
}

/// Build one scalar expression node. `group` and `in` are aggregate/window
/// lowering, not relational structure, and are unchanged: the constructor
/// carries the same phase rule the inline check did.
fn expr_prim(p: Prim, next: &mut impl FnMut() -> Value) -> EResult<CoreTerm> {
    match p {
        Prim::In => {
            let values = expressions(next())?;
            let value = core_expr(next())?;
            core(CoreTerm::in_(value, values, false))
        }
        Prim::Group => Ok(core(CoreTerm::group(core_expr(next())?))?),
        _ => unreachable!("`{}` does not build an expression", prim_name(p)),
    }
}


/// The name a primitive is reported under, for the `unreachable!` messages
/// above. It is a debug aid: those arms are unreachable by `classify`.
fn prim_name(p: Prim) -> &'static str {
    crate::value::PRIMS
        .iter()
        .find(|(_, q)| *q == p)
        .map(|(n, _)| *n)
        .unwrap_or("?")
}

fn frame_prim(next: &mut impl FnMut() -> Value) -> EResult<Frame> {
    let (start, end) = (bound(next())?, bound(next())?);
    if start == Bound::UnboundedFollowing
        || end == Bound::UnboundedPreceding
        || start.key() > end.key()
    {
        return err(format!("impossible window frame: {start:?} to {end:?}"));
    }
    Ok(Frame { start, end })
}

fn bound_prim(p: Prim, next: &mut impl FnMut() -> Value) -> EResult<Bound> {
    Ok(match p {
        Prim::UnboundedPreceding => Bound::UnboundedPreceding,
        Prim::UnboundedFollowing => Bound::UnboundedFollowing,
        Prim::CurrentRow => Bound::CurrentRow,
        Prim::Preceding => Bound::Preceding(count(next(), "preceding")?),
        Prim::Following => Bound::Following(count(next(), "following")?),
        _ => unreachable!("`{}` does not build a frame bound", prim_name(p)),
    })
}
