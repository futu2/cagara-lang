//! The non-relational half of the language: primitives and `sql` templates.
//!
//! A scalar expression is not reduced to a number inside the compiler — its
//! *meaning* is a column, a literal, a `group`, an `in`, or a SQL template over
//! arguments, and that is what the checked layer holds. Saturation is the
//! interesting step: a template that has all its arguments becomes a
//! [`CheckedExpr`], which is where its phase is fixed from its arguments'
//! phases.
//!
//! The prelude's `group` and `in` take expressions rather than query operands,
//! and they reduce by the same rule as everything else: an application of
//! `group` to an expression is a `group` node, not a function that happened to
//! be called.

use cagara_syntax::ast::{Expr, ExprKind, Lit};

use super::value::{stage_spec, value_class};
use super::{frame, CombineFn, Control, Env, Normalizer, StageFn, TemplateFn, Value};
use crate::check::TemplateKind;
use crate::checked::CheckedExpr;
use crate::core::{Origin, ScalarType};

/// The value of a `primitive "…"` body.
pub(crate) fn primitive<'a>(
    ev: &mut Normalizer<'a, 'a>,
    module: usize,
    expr: &'a Expr,
    name: &str,
) -> Result<Value<'a>, Control> {
    let origin = Origin::new(module, expr.span);
    let Some(prim) = crate::primitive::prim_of(name) else {
        return Err(Control::unsupported(
            origin,
            format!("`{name}` is not a known primitive"),
        ));
    };
    if let Some(spec) = stage_spec(prim) {
        return Ok(Value::Stage(StageFn {
            spec,
            args: Vec::new(),
        }));
    }
    if let Some(value) = frame::primitive_value(prim, origin) {
        return Ok(value);
    }
    match prim {
        crate::primitive::Prim::Join(kind) => Ok(Value::Combine(CombineFn::Join {
            kind,
            args: Vec::new(),
        })),
        crate::primitive::Prim::Set(kind) => Ok(Value::Combine(CombineFn::Set {
            kind,
            args: Vec::new(),
        })),
        // `table "s" "t"`: the only leaf. Its two arguments are labels; the
        // columns are the row the checker closed for the call.
        crate::primitive::Prim::Table => Ok(Value::Table {
            args: Vec::new(),
            row: ev.row(module, expr),
            origin,
        }),
        // `asc` / `desc`: an ordering whose direction is the primitive it
        // resolved to, not the word `asc` in some source file.
        crate::primitive::Prim::Asc => Ok(Value::Direction {
            ascending: true,
            args: Vec::new(),
            origin,
        }),
        crate::primitive::Prim::Desc => Ok(Value::Direction {
            ascending: false,
            args: Vec::new(),
            origin,
        }),
        crate::primitive::Prim::Group => Ok(Value::Group { arg: None, origin }),
        crate::primitive::Prim::In => Ok(Value::Membership {
            list: None,
            value: None,
            origin,
        }),
        // `merge` has no relational form in this language: two queries are
        // combined by a join or a set operation, and `merge r s` is a row
        // *type* former rather than an operation.
        other => Err(Control::unsupported(
            origin,
            format!("the primitive `{name}` ({other:?}) has no relational lowering"),
        )),
    }
}

/// The value of a `sql "…"` body, with the arity its definition declares.
pub(crate) fn template_value<'a>(
    ev: &mut Normalizer<'a, 'a>,
    module: usize,
    expr: &'a Expr,
    sql: &str,
    def: usize,
) -> Result<Value<'a>, Control> {
    let origin = Origin::new(module, expr.span);
    let kind = ev.declared_template_kind(module, def);
    let arity = ev.declared_arity(module, def);
    let ty = ev.ty(module, expr).unwrap_or(ScalarType::Unknown);
    if arity == 0 {
        // A template with no arguments is already saturated: applying it is not
        // a step, so the value *is* the checked expression.
        return build(sql, kind, None, Vec::new(), origin, ty);
    }
    Ok(Value::Template(TemplateFn {
        sql: sql.to_string(),
        kind,
        arity,
        spec: None,
        args: Vec::new(),
        origin,
        ty,
    }))
}

/// The value of a `sql "…"` written as an expression rather than as a body.
///
/// A template used directly is a partial application: the *application*
/// supplies its arguments, and the checker recorded the application's type at
/// that site, so the template itself carries no arity and no type.
pub(crate) fn template<'a>(
    _ev: &mut Normalizer<'a, 'a>,
    module: usize,
    expr: &'a Expr,
    sql: &str,
) -> Result<Value<'a>, Control> {
    Ok(Value::Template(TemplateFn {
        sql: sql.to_string(),
        kind: TemplateKind::Scalar,
        arity: usize::MAX,
        spec: None,
        args: Vec::new(),
        origin: Origin::new(module, expr.span),
        ty: ScalarType::Unknown,
    }))
}

/// A template that has received one more argument.
pub(crate) fn apply_template<'a>(
    ev: &mut Normalizer<'a, 'a>,
    mut template: TemplateFn,
    arg: Value<'a>,
) -> Result<Value<'a>, Control> {
    // A window template's first argument is its *specification*, not a scalar:
    // the record is reduced here by the rule its fields have, and what the
    // template consumes is the finished specification.
    if template.kind == TemplateKind::Win && template.spec.is_none() {
        let Value::WinSpec(spec) = window_spec(ev, ev.call_module(), ev.call_expr()?, &arg)? else {
            unreachable!("`window_spec` builds a specification")
        };
        template.spec = Some(spec);
        return if template.args.len() >= template.arity {
            saturated(ev, template)
        } else {
            Ok(Value::Template(template))
        };
    }
    let expr = ev.as_expr(arg, ev.call_module(), ev.call_expr()?)?;
    template.args.push(expr);
    if template.args.len() >= template.arity {
        return saturated(ev, template);
    }
    Ok(Value::Template(template))
}

/// Build the checked expression a saturated template denotes.
fn saturated<'a>(ev: &Normalizer<'a, 'a>, template: TemplateFn) -> Result<Value<'a>, Control> {
    let ty = match template.ty {
        ScalarType::Unknown => ev.ty_at(template.origin).unwrap_or(ScalarType::Unknown),
        known => known,
    };
    build(
        &template.sql,
        template.kind,
        template.spec,
        template.args,
        template.origin,
        ty,
    )
}

fn build<'a>(
    sql: &str,
    kind: TemplateKind,
    spec: Option<Box<crate::checked::WinSpecChecked>>,
    args: Vec<CheckedExpr>,
    origin: Origin,
    ty: ScalarType,
) -> Result<Value<'a>, Control> {
    let built = match kind {
        TemplateKind::Scalar => CheckedExpr::template(sql, args, ty, origin),
        TemplateKind::Agg => CheckedExpr::agg_template(sql, args, ty, origin),
        TemplateKind::Win => match spec {
            Some(spec) => CheckedExpr::win_template(sql, args, *spec, ty, origin),
            None => {
                return Err(Control::unsupported(
                    origin,
                    "a window function without a specification",
                ))
            }
        },
    };
    built.map(Value::Scalar).map_err(Control::Program)
}

/// A `case { … }`: a searched `CASE`, with its arms in source order and the
/// last one as the default.
pub(crate) fn case<'a>(
    ev: &mut Normalizer<'a, 'a>,
    module: usize,
    expr: &'a Expr,
    env: &Env<'a>,
) -> Result<Value<'a>, Control> {
    let origin = Origin::new(module, expr.span);
    let ExprKind::Case(arms) = &expr.kind else {
        unreachable!("the caller matched a case")
    };
    let Some(((default_condition, default_expr), when_arms)) = arms.split_last() else {
        return Err(Control::unsupported(origin, "a `case` with no arms"));
    };
    if !matches!(default_condition.kind, ExprKind::Lit(Lit::Bool(true))) {
        return Err(Control::unsupported(
            origin,
            "a `case` whose last arm is not the default",
        ));
    }
    if when_arms.is_empty() {
        return ev.value(module, default_expr, env);
    }
    let mut args = Vec::with_capacity(when_arms.len() * 2 + 1);
    let mut sql = String::from("CASE");
    // An arm is an argument of the `CASE` template, and a literal arm takes
    // that template's phase exactly as a literal argument to any other
    // template does.
    let phase = ev.call_site_phase().unwrap_or(crate::ir::Phase::Const);
    for (condition, value) in when_arms {
        let condition_value = ev.value(module, condition, env)?;
        let condition = ev.as_expr(condition_value, module, condition)?;
        let arm_value = ev.value(module, value, env)?;
        let value = ev.as_expr(arm_value, module, value)?;
        args.push(super::value::rephase(condition, phase));
        let condition_index = args.len();
        args.push(super::value::rephase(value, phase));
        let value_index = args.len();
        sql.push_str(&format!(" WHEN ${condition_index} THEN ${value_index}"));
    }
    let default_value = ev.value(module, default_expr, env)?;
    let default = ev.as_expr(default_value, module, default_expr)?;
    args.push(super::value::rephase(default, phase));
    let default_index = args.len();
    sql.push_str(&format!(" ELSE ${default_index} END"));
    let Some(ty) = ev.ty(module, expr) else {
        return Err(Control::unsupported(
            origin,
            "a `case` without a checked type",
        ));
    };
    CheckedExpr::template(sql, args, ty, origin)
        .map(Value::Scalar)
        .map_err(Control::Program)
}

/// A `winspec` record as a complete window specification.
///
/// The record's fields and their meanings are the prelude's (`rules::winspec`),
/// and its values are the ones already reduced into it: a list of expressions
/// for `partition`, a list of sort keys for `order`, and a frame. The frame and
/// its bounds are the frame module's; this only assembles them, and the
/// validation is [`crate::checked::window_spec`]'s, so a specification cannot be
/// valid here and invalid in the checked layer.
pub(crate) fn window_spec<'a>(
    ev: &Normalizer<'a, 'a>,
    module: usize,
    expr: &'a Expr,
    record: &Value<'_>,
) -> Result<Value<'a>, Control> {
    use crate::rules::winspec;
    let origin = Origin::new(module, expr.span);
    let Value::Tuple { items, names } = record else {
        return Err(Control::unsupported(
            origin,
            "a window specification is a record",
        ));
    };
    let Some(names) = names else {
        return Err(Control::unsupported(
            origin,
            "a window specification is a record",
        ));
    };
    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    for (name, value) in names.iter().zip(items.iter()) {
        match name.as_str() {
            winspec::PARTITION => {
                let Value::Tuple { items, .. } = value else {
                    return Err(Control::unsupported(origin, "`partition` is a list"));
                };
                for item in items {
                    partition.push(ev.as_expr(item.clone(), module, expr)?);
                }
            }
            winspec::ORDER => {
                let Value::Tuple { items, .. } = value else {
                    return Err(Control::unsupported(origin, "`order` is a list"));
                };
                for item in items {
                    match item {
                        Value::SortKey(key, ascending) => order.push((key.clone(), *ascending)),
                        other => order.push((ev.as_expr(other.clone(), module, expr)?, true)),
                    }
                }
            }
            winspec::FRAME => match value {
                Value::Frame(built) => frame = Some(*built),
                other => {
                    return Err(Control::unsupported(
                        origin,
                        format!("`frame` is a row frame, and this is {}", value_class(other)),
                    ))
                }
            },
            // An unknown key is the checker's to reject for the record type it
            // gave the specification; here it is a capability gap rather than a
            // guess about what the field was meant to be.
            other => {
                return Err(Control::unsupported(
                    origin,
                    format!("`{other}` is not a window specification field"),
                ))
            }
        }
    }
    crate::checked::window_spec(partition, order, frame)
        .map(|spec| Value::WinSpec(Box::new(spec)))
        .map_err(Control::Program)
}
