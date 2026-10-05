//! Rust implementations of the `__` primitives and of `sql` template
//! saturation. Phase rules are checked as soon as an expression is built, so
//! errors point at the offending application.

use crate::ir::{Bound, Expr, Frame, Lit, Rel, WinSpec};
use crate::value::{err, EResult, Prim, Template, TplKind, Value};

pub fn lift(v: Value) -> EResult<Expr> {
    match v {
        Value::Lit(l) => Ok(Expr::Lit(l)),
        Value::Expr(e) => Ok(e),
        o => err(format!(
            "expected a column expression or constant, found {}",
            o.kind()
        )),
    }
}

fn lift_all(vs: Vec<Value>) -> EResult<Vec<Expr>> {
    vs.into_iter().map(lift).collect()
}

fn checked(e: Expr) -> EResult<Value> {
    match e.phase() {
        Ok(_) => Ok(Value::Expr(e)),
        Err(m) => err(m),
    }
}

pub fn build_tpl(t: &Template, args: Vec<Value>) -> EResult<Value> {
    let sql = t.sql.clone();
    match t.kind {
        // A scalar template is top-level like any other expression: it may
        // wrap a window or an aggregate, but it cannot be mixed with one. If
        // its arguments already are one, the whole template is that phase,
        // which keeps `inc : agg (expr r int) -> expr r int = sql "$1 + 1"`
        // an aggregate rather than an aggregate inside a scalar.
        TplKind::Scalar => checked(Expr::Tpl(sql, lift_all(args)?)),
        TplKind::Agg => checked(Expr::Agg(sql, lift_all(args)?)),
        TplKind::Win => {
            let mut it = args.into_iter();
            let spec = match it.next() {
                Some(s) => win_spec(s)?,
                None => return err("window function without a spec argument"),
            };
            checked(Expr::Win(sql, lift_all(it.collect())?, Box::new(spec)))
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

fn expressions(v: Value) -> EResult<Vec<Expr>> {
    list(v)?.into_iter().map(lift).collect()
}

fn query(v: Value) -> EResult<Rel> {
    match v {
        Value::Query(r) => Ok(r),
        o => err(format!("expected a query, found {}", o.kind())),
    }
}

fn count(v: Value, what: &str) -> EResult<i64> {
    match v {
        Value::Lit(Lit::Int(n)) if n >= 0 => Ok(n),
        Value::Lit(Lit::Int(n)) => err(format!("`{what}` needs a non-negative count, got {n}")),
        o => err(format!("`{what}` expects an int, found {}", o.kind())),
    }
}

fn fields(v: Value) -> EResult<Vec<(String, Expr)>> {
    match v {
        Value::Record(fs) => fs
            .into_iter()
            .map(|(k, v)| {
                let e = lift(v).map_err(|mut e| {
                    e.message = format!("field `{k}`: {}", e.message);
                    e
                })?;
                Ok((k, e))
            })
            .collect(),
        o => err(format!(
            "expected a record of column expressions, found {}",
            o.kind()
        )),
    }
}

pub fn sort_key(v: Value) -> EResult<(Expr, bool)> {
    match v {
        Value::Dir(e, asc) => Ok((e, asc)),
        o => Ok((lift(o)?, true)),
    }
}

fn bound(v: Value) -> EResult<Bound> {
    match v {
        Value::Bound(b) => Ok(b),
        o => err(format!("expected a frame bound, found {}", o.kind())),
    }
}

fn win_spec(v: Value) -> EResult<WinSpec> {
    let Value::Record(fs) = v else {
        return err(format!(
            "a window function expects {{ {} }}, found {}",
            crate::rules::winspec::names(),
            v.kind()
        ));
    };
    let mut spec = WinSpec {
        partition: vec![],
        order: vec![],
        frame: None,
    };
    for (k, v) in fs {
        match k.as_str() {
            crate::rules::winspec::PARTITION => spec.partition = lift_all(list(v)?)?,
            crate::rules::winspec::ORDER => {
                spec.order = list(v)?.into_iter().map(sort_key).collect::<EResult<_>>()?
            }
            crate::rules::winspec::FRAME => match v {
                Value::Frame(f) => spec.frame = Some(f),
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
    Ok(spec)
}

/// Apply a saturated primitive. Argument order follows the prelude's
/// curried signatures, with the query argument last so `q & where p` works.
pub fn call(p: Prim, args: Vec<Value>) -> EResult<Value> {
    let mut a = args.into_iter();
    let mut next = || a.next().expect("primitive called with its full arity");
    Ok(match p {
        Prim::Table => {
            let schema = string(next())?;
            let name = string(next())?;
            Value::Query(Rel::Table {
                schema,
                name,
                columns: None,
            })
        }
        Prim::Where => {
            let pred = lift(next())?;
            pred.phase().or_else(err)?;
            Value::Query(Rel::Where(Box::new(query(next())?), pred))
        }
        Prim::Select => {
            let fs = fields(next())?;
            Value::Query(Rel::Select(Box::new(query(next())?), fs))
        }
        Prim::Update => {
            let fs = fields(next())?;
            Value::Query(Rel::Update(Box::new(query(next())?), fs))
        }
        Prim::Omit => {
            let key = string(next())?;
            Value::Query(Rel::Omit(Box::new(query(next())?), key))
        }
        Prim::Prefix => {
            let affix = string(next())?;
            Value::Query(Rel::Prefix(Box::new(query(next())?), affix))
        }
        Prim::Suffix => {
            let affix = string(next())?;
            Value::Query(Rel::Suffix(Box::new(query(next())?), affix))
        }
        Prim::AggStage => {
            let fs = fields(next())?;
            Value::Query(Rel::Agg(Box::new(query(next())?), fs))
        }
        Prim::Order => {
            let keys = list(next())?
                .into_iter()
                .map(sort_key)
                .collect::<EResult<Vec<_>>>()?;
            Value::Query(Rel::Order(Box::new(query(next())?), keys))
        }
        Prim::Limit => {
            let n = count(next(), "limit")?;
            Value::Query(Rel::Limit(Box::new(query(next())?), n))
        }
        Prim::Offset => {
            let n = count(next(), "offset")?;
            Value::Query(Rel::Offset(Box::new(query(next())?), n))
        }
        Prim::Distinct => Value::Query(Rel::Distinct(Box::new(query(next())?))),
        Prim::In => {
            let values = expressions(next())?;
            let value = lift(next())?;
            checked(Expr::In(Box::new(value), values, false))?
        }
        Prim::Join(kind) => {
            let right = query(next())?;
            let on = lift(next())?;
            let left = query(next())?;
            Value::Query(Rel::Join {
                kind,
                left: Box::new(left),
                right: Box::new(right),
                on,
            })
        }
        Prim::Set(kind) => {
            let left = query(next())?;
            let right = query(next())?;
            Value::Query(Rel::Set {
                kind,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Prim::Group => checked(Expr::Group(Box::new(lift(next())?)))?,
        Prim::Asc => Value::Dir(lift(next())?, true),
        Prim::Desc => Value::Dir(lift(next())?, false),
        Prim::Rows => {
            let (start, end) = (bound(next())?, bound(next())?);
            if start == Bound::UnboundedFollowing
                || end == Bound::UnboundedPreceding
                || start.key() > end.key()
            {
                return err(format!("impossible window frame: {start:?} to {end:?}"));
            }
            Value::Frame(Frame { start, end })
        }
        Prim::UnboundedPreceding => Value::Bound(Bound::UnboundedPreceding),
        Prim::UnboundedFollowing => Value::Bound(Bound::UnboundedFollowing),
        Prim::CurrentRow => Value::Bound(Bound::CurrentRow),
        Prim::Preceding => Value::Bound(Bound::Preceding(count(next(), "preceding")?)),
        Prim::Following => Value::Bound(Bound::Following(count(next(), "following")?)),
        Prim::MapValue => {
            return err(
                "mapValue is a type-level operation and should be resolved during type checking"
                    .to_string(),
            )
        }
        Prim::Merge => {
            return err(
                "merge is a type-level operation and should be resolved during type checking"
                    .to_string(),
            )
        }
    })
}
