//! Window frames: `rows`, `preceding`, `following`, and the fixed bounds.
//!
//! A frame is not a value the language computes with; it is a *shape* the
//! checker validates and the SQL backend reads. Reducing it therefore produces
//! a [`Bound`](crate::ir::Bound) or a [`Frame`](crate::ir::Frame), and the
//! validation is the checked layer's own ([`crate::checked::frame`]) rather
//! than a second copy of it.

use crate::core::Origin;
use crate::ir::Bound;

use super::{Control, Label, Normalizer, Value};
use crate::checked::CheckedExprNode;

/// The fold an accumulator performs once its last argument arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldSpec {
    /// `rows start end`.
    Rows,
    /// `preceding n` / `following n`.
    Offset { following: bool },
}

/// The value of a frame primitive: a fixed bound, or an accumulator waiting for
/// its offsets.
pub(crate) fn primitive_value<'a>(
    prim: crate::primitive::Prim,
    origin: Origin,
) -> Option<Value<'a>> {
    use crate::primitive::Prim;
    Some(match prim {
        Prim::UnboundedPreceding => Value::Bound(Bound::UnboundedPreceding),
        Prim::UnboundedFollowing => Value::Bound(Bound::UnboundedFollowing),
        Prim::CurrentRow => Value::Bound(Bound::CurrentRow),
        Prim::Rows => Value::Fold {
            partial: Vec::new(),
            origin,
            spec: FoldSpec::Rows,
        },
        Prim::Preceding => Value::Fold {
            partial: Vec::new(),
            origin,
            spec: FoldSpec::Offset { following: false },
        },
        Prim::Following => Value::Fold {
            partial: Vec::new(),
            origin,
            spec: FoldSpec::Offset { following: true },
        },
        _ => return None,
    })
}

/// Apply one argument to a frame accumulator, constructing the frame once it is
/// saturated.
pub(crate) fn fold<'a>(
    _ev: &Normalizer<'_, 'a>,
    partial: &[Value<'static>],
    spec: FoldSpec,
    arg: Value<'a>,
    origin: Origin,
) -> Result<Value<'a>, Control> {
    let mut args = partial.to_vec();
    args.push(super::value::own(&arg));
    let arity = match spec {
        FoldSpec::Rows => 2,
        FoldSpec::Offset { .. } => 1,
    };
    if args.len() < arity {
        return Ok(Value::Fold {
            partial: args,
            origin,
            spec,
        });
    }
    match spec {
        FoldSpec::Rows => {
            let (Some(Value::Bound(start)), Some(Value::Bound(end))) = (args.first(), args.get(1))
            else {
                return Err(Control::unsupported(
                    origin,
                    "`rows` takes two frame bounds",
                ));
            };
            let (start, end) = (*start, *end);
            crate::checked::frame(start, end)
                .map(Value::Frame)
                .map_err(Control::Program)
        }
        FoldSpec::Offset { following } => {
            let n = match args.first() {
                Some(Value::Label(Label { int: Some(n), .. })) => *n,
                Some(Value::Scalar(expr)) => {
                    let (_, _, node, _) = expr.clone().into_parts();
                    match node {
                        CheckedExprNode::Lit(crate::ir::Lit::Int(n)) => n,
                        _ => {
                            return Err(Control::unsupported(
                                origin,
                                "a frame offset takes an int literal",
                            ))
                        }
                    }
                }
                _ => {
                    return Err(Control::unsupported(
                        origin,
                        "a frame offset takes an int literal",
                    ))
                }
            };
            Ok(Value::Bound(if following {
                Bound::Following(n)
            } else {
                Bound::Preceding(n)
            }))
        }
    }
}
