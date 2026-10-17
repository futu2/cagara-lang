//! Tree-driven query lowering slices.
//!
//! This builder consumes checker-owned stage records, expression facts, and
//! typed query rows. It handles direct pipelines and recursively composed
//! joins and set operations over tables or typed query aliases. Shapes it
//! cannot construct return `None`; the production coordinator turns that into
//! a compiler capability diagnostic. The normalized term implementation is
//! used only by agreement tests.
#![allow(dead_code)] // Keep the slice self-contained while later stages land.

use crate::check::{CombineKind, NodeId, NodeRef, QueryNodeKind, Row, RowStage, TypedQuery};
use crate::checked::CheckedQuery;
use crate::compile::CompilerInput;
use crate::core::{Error, Origin, RowType};
use crate::normalize::Resolved;
use cagara_syntax::ast::{Expr, ExprKind, Lit};
use std::collections::{HashMap, HashSet};

/// State for one typed-query lowering attempt.
///
/// Lowering follows aliases and helper definitions, so it has a graph-shaped
/// dependency walk even though the source definition is a tree. This state is
/// deliberately owned by the attempt and threaded through the builders. A
/// thread-local active set made the result depend on ambient execution state
/// and could not be reused by an incremental compiler or a concurrent request.
struct LoweringContext {
    active: HashSet<(usize, usize)>,
    query_bindings: HashMap<(usize, u32), (TypedQuery, CheckedQuery)>,
    stage_bindings: HashMap<(usize, u32), (usize, Expr)>,
}

impl LoweringContext {
    fn new() -> Self {
        Self {
            active: HashSet::new(),
            query_bindings: HashMap::new(),
            stage_bindings: HashMap::new(),
        }
    }

    fn lower(
        &mut self,
        input: CompilerInput<'_>,
        module: usize,
        def: usize,
        body: &Expr,
    ) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
        Ok(self
            .lower_with_status(input, module, def, body)?
            .map(|(typed, checked, _)| (typed, checked)))
    }

    fn lower_with_status(
        &mut self,
        input: CompilerInput<'_>,
        module: usize,
        def: usize,
        body: &Expr,
    ) -> Result<Option<(TypedQuery, CheckedQuery, bool)>, Error> {
        let key = (module, def);
        if !self.active.insert(key) {
            return Err(Error::unsupported("query definition refers to itself")
                .at(Origin::new(module, body.span)));
        }
        let result = lower_impl(self, input, module, def, body);
        self.active.remove(&key);
        result
    }
}

/// Build the typed query and its checked counterpart when this slice applies.
pub(crate) fn lower(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    LoweringContext::new().lower(input, module, def, body)
}

/// Build the typed query and report whether its graph aligned with the checked
/// adapter tree well enough to reconstruct its checked nodes.
pub(crate) fn lower_with_status(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery, bool)>, Error> {
    LoweringContext::new().lower_with_status(input, module, def, body)
}

fn lower_impl(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery, bool)>, Error> {
    let Some((mut typed, checked)) = lower_unhydrated(context, input, module, def, body)? else {
        return Ok(None);
    };
    typed
        .carry_checked_payloads(&checked)
        .map_err(query_error)?;
    typed.require_checked_payloads().map_err(query_error)?;
    let rebuilt = typed.rebuild_checked(&checked).map_err(query_error)?;
    match rebuilt {
        Some(checked) => Ok(Some((typed, checked, true))),
        None => Ok(Some((typed, checked, false))),
    }
}

fn lower_unhydrated(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    if query_target(tc, module, body) == Some((module, def)) {
        return Err(Error::unsupported("query definition refers to itself")
            .at(Origin::new(module, body.span)));
    }

    // A standalone join/set can be rebuilt from its typed operands before the
    // general composed-pipeline route handles surrounding stages.
    if let Some(lowered) = lower_combine_pipeline(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }

    // A combine can occur below or above ordinary stages. Build the complete
    // relational graph recursively so a stage never causes a combine child to
    // be dropped.
    if contains_combine_tree(input, module, body) || contains_join_operator_tree(body) {
        if let Some(lowered) = lower_composed_pipeline(context, input, module, def, body)? {
            return Ok(Some(lowered));
        }
    }

    // `distinct` is row preserving and consequently has no checker row-stage
    // record. Route pipelines containing it through the source composer so it
    // cannot disappear when another recorded stage surrounds it.
    if contains_distinct_stage(input, module, body) {
        let mut typed = TypedQuery::new();
        if let Some((root, checked)) =
            lower_plain_composed_expr(context, input, module, def, body, &mut typed)?
        {
            return Ok(Some((typed.finish(root), checked)));
        }
    }

    if let Some(lowered) = lower_keep_pipeline(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_projection_pipeline(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_where_pipeline(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_order_pipeline(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_plain_expression_chain(context, input, module, def, body)? {
        return Ok(Some(lowered));
    }
    // Non-direct stage values (named/composed stages and inline lambdas) do
    // not leave caller-owned row-stage records. Walk their source structure
    // through the same typed composer before considering helper fallback.
    if !is_direct_pipeline(input, module, body) {
        let mut typed = TypedQuery::new();
        if let Some((root, checked)) =
            lower_plain_composed_expr(context, input, module, def, body, &mut typed)?
        {
            return Ok(Some((typed.finish(root), checked)));
        }
    }
    // A helper application may itself carry recorded stages at the use site
    // (for example `h (where ...)`). It is not a direct source pipeline, so
    // the stage-record path below would otherwise decline it before the
    // checker-driven helper instantiator gets a chance to bind its parameters.
    if !is_direct_pipeline(input, module, body) {
        if let Some(lowered) = lower_helper_pipeline(context, input, module, def, body)? {
            return Ok(Some(lowered));
        }
    }
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    // A name or qualified projection can be an alias of a query definition.
    // Instantiate the already-typed target tree at this use while preserving
    // the target's stable stage identities. Plain table aliases fall through:
    // they have no stages and are handled by `table_parts` below.
    if stages.is_empty() {
        if let Some(lowered) = lower_helper_pipeline(context, input, module, def, body)? {
            return Ok(Some(lowered));
        }
        if let Some((target_module, target_def)) = query_target(tc, module, body) {
            if (target_module, target_def) != (module, def)
                && matches!(
                    tc.scheme_view(target_module, target_def),
                    Some(crate::check::SchemeView::Query { .. })
                )
            {
                let target_body = &input.module(target_module).def(target_def).body;
                if let Some(lowered) =
                    context.lower(input, target_module, target_def, target_body)?
                {
                    return Ok(Some(lowered));
                }
            }
        }
    }
    // A definition with no recorded rewrite stage may still contain a
    // relational stage whose signature has no label witness (`where`,
    // `select`, `order`, ...). The first slice must decline such a pipeline
    // instead of silently dropping that operation.
    if stages.is_empty() || contains_unmigrated_stage(input, module, body) {
        return Ok(None);
    }
    if stages.iter().any(|stage| {
        stage.node.definition_module != module
            || stage.node.definition != def
            || !matches!(
                stage.kind,
                RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
            )
            || stage.input.is_none()
            || stage.output.is_none()
            || stage.label.is_none()
    }) {
        return Ok(None);
    }

    let mut syntax = HashMap::new();
    index_exprs(body, &mut syntax);
    let stage_sites: HashSet<u32> = stages.iter().map(|stage| stage.site).collect();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let Some((schema, table, columns)) = table_parts(input, module, base) else {
        return Ok(None);
    };
    let base_origin = Origin::new(module, base.span);
    let checked_columns = RowType::new(columns.clone());
    let mut checked = CheckedQuery::table(schema, table, Some(checked_columns), base_origin)
        .map_err(|error| Error::unsupported(error.message).at(base_origin))?;

    let mut typed = TypedQuery::new();
    let base_id = typed
        .table(
            NodeRef::new(module, def, base.id),
            base_origin,
            columns.clone(),
        )
        .map_err(query_error)?;
    let mut child = base_id;

    for stage in stages {
        let origin = Origin::new(
            module,
            syntax.get(&stage.site).map_or(stage.span, |e| e.span),
        );
        // For expression stages the checker may close an unordered row
        // constraint in a different field order than the query child. The
        // child is the typed boundary this node actually consumes; rewrites
        // retain their strict recorded row because their mapper is order
        // sensitive.
        let child_row = typed
            .node(child)
            .output_row()
            .cloned()
            .ok_or_else(|| Error::unsupported("typed stage child has no output row"))?;
        let input_row = if matches!(
            stage.kind,
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
        ) {
            Row::new(stage.input.clone().expect("checked above"))
        } else {
            child_row
        };
        let output_row = Row::new(stage.output.clone().expect("checked above"));
        let kind = match stage.kind {
            RowStage::Prefix => QueryNodeKind::Prefix,
            RowStage::Suffix => QueryNodeKind::Suffix,
            RowStage::Omit => QueryNodeKind::Omit,
            RowStage::MapValue => QueryNodeKind::MapValue,
            _ => unreachable!("filtered above"),
        };
        let id = typed
            .rewrite(
                stage.node,
                kind,
                child,
                Some(input_row),
                Some(output_row.clone()),
                origin,
            )
            .map_err(query_error)?;
        let label = stage.label.expect("checked above");
        checked = match stage.kind {
            RowStage::Prefix => CheckedQuery::prefix(checked, label, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?,
            RowStage::Suffix => CheckedQuery::suffix(checked, label, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?,
            RowStage::Omit => CheckedQuery::omit(checked, label, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?,
            RowStage::MapValue => CheckedQuery::map_value(checked, label, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?,
            _ => unreachable!("filtered above"),
        };
        if checked.row().columns() != output_row.columns() {
            return Err(Error::unsupported(format!(
                "typed stage {:?} produced a row different from its checked record",
                stage.kind
            ))
            .at(origin));
        }
        child = id;
    }

    Ok(Some((typed.finish(child), checked)))
}

/// Lower a direct projection whose fields use the plain checked expression
/// subset. This small expression slice needs no normalized term: the checker
/// supplies each expression's scalar type and the stage record supplies the
/// closed output row.
fn lower_simple_projection_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.len() != 1
        || !matches!(
            stages[0].kind,
            RowStage::Select | RowStage::Update | RowStage::Agg
        )
        || stages[0].node.definition_module != module
        || stages[0].node.definition != def
        || !is_direct_pipeline(input, module, body)
        || contains_other_stage(input, module, body, stages[0].kind)
    {
        return Ok(None);
    }
    let stage = &stages[0];
    let stage_sites = std::iter::once(stage.site).collect::<HashSet<_>>();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let stage_expr = find_expr_id(body, stage.site).unwrap_or(body);
    let ExprKind::App(_, args) = &stage_expr.kind else {
        return Ok(None);
    };
    let Some(record) = args.first() else {
        return Ok(None);
    };
    let ExprKind::Record(fields) = &record.kind else {
        return Ok(None);
    };
    let mut checked_fields = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let Some(checked) = lower_plain_expr(input, module, value)? else {
            return Ok(None);
        };
        checked_fields.push((name.clone(), checked));
    }
    let stage_origin = Origin::new(module, stage_expr.span);
    let (child, mut checked, mut typed) =
        if let Some((schema, table, columns)) = table_parts(input, module, base) {
            let checked = CheckedQuery::table(
                schema,
                table,
                Some(RowType::new(columns.clone())),
                stage_origin,
            )
            .map_err(|error| Error::unsupported(error.message).at(stage_origin))?;
            let mut typed = TypedQuery::new();
            let child = typed
                .table(NodeRef::new(module, def, base.id), stage_origin, columns)
                .map_err(query_error)?;
            (child, checked, typed)
        } else if let Some((target_module, target_def)) = query_target(tc, module, base) {
            if (target_module, target_def) == (module, def)
                || !matches!(
                    tc.scheme_view(target_module, target_def),
                    Some(crate::check::SchemeView::Query { .. })
                )
            {
                return Ok(None);
            }
            let target_body = &input.module(target_module).def(target_def).body;
            let Some((target_typed, checked)) =
                context.lower(input, target_module, target_def, target_body)?
            else {
                return Ok(None);
            };
            let mut typed = TypedQuery::new();
            let child = copy_typed_graph(&target_typed, &mut typed)?;
            (child, checked, typed)
        } else {
            return Ok(None);
        };
    let output_row = stage.output.clone().map(Row::new);
    let kind = match stage.kind {
        RowStage::Select => QueryNodeKind::Select,
        RowStage::Update => QueryNodeKind::Update,
        RowStage::Agg => QueryNodeKind::Agg,
        _ => unreachable!(),
    };
    let mut id = typed
        .project(
            stage.node,
            kind,
            child,
            output_row,
            stage_origin,
            Some(checked_fields.clone()),
        )
        .map_err(query_error)?;
    checked = match stage.kind {
        RowStage::Select => CheckedQuery::select(checked, checked_fields, stage_origin),
        RowStage::Update => CheckedQuery::update(checked, checked_fields, stage_origin),
        RowStage::Agg => CheckedQuery::agg(checked, checked_fields, stage_origin),
        _ => unreachable!(),
    }
    .map_err(|error| Error::unsupported(error.message).at(stage_origin))?;
    if let Some(distinct) = find_pipeline_stage(input, module, body, RowStage::Distinct) {
        let origin = Origin::new(module, distinct.span);
        id = typed
            .keep_row(
                NodeRef::new(module, def, distinct.id),
                QueryNodeKind::Distinct,
                id,
                origin,
            )
            .map_err(query_error)?;
        checked = CheckedQuery::distinct(checked, origin)
            .map_err(|error| Error::unsupported(error.message).at(origin))?;
    }
    Ok(Some((typed.finish(id), checked)))
}

/// Lower a direct filter whose predicate uses only fields, literals, and the
/// scalar comparison/boolean operators declared in the prelude.
fn lower_simple_where_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.len() != 1
        || stages[0].kind != RowStage::Where
        || stages[0].node.definition_module != module
        || stages[0].node.definition != def
        || !is_direct_pipeline(input, module, body)
        || contains_other_stage(input, module, body, RowStage::Where)
    {
        return Ok(None);
    }
    let stage = &stages[0];
    let stage_sites = std::iter::once(stage.site).collect::<HashSet<_>>();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let stage_expr = find_expr_id(body, stage.site).unwrap_or(body);
    let ExprKind::App(_, args) = &stage_expr.kind else {
        return Ok(None);
    };
    let Some(predicate_expr) = args.first() else {
        return Ok(None);
    };
    let Some(predicate) = lower_plain_expr(input, module, predicate_expr)? else {
        return Ok(None);
    };
    let origin = Origin::new(module, stage_expr.span);
    let (child, checked, mut typed) =
        if let Some((schema, table, columns)) = table_parts(input, module, base) {
            let checked =
                CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                    .map_err(|error| Error::unsupported(error.message).at(origin))?;
            let mut typed = TypedQuery::new();
            let child = typed
                .table(NodeRef::new(module, def, base.id), origin, columns)
                .map_err(query_error)?;
            (child, checked, typed)
        } else if let Some((target_module, target_def)) = query_target(tc, module, base) {
            if (target_module, target_def) == (module, def)
                || !matches!(
                    tc.scheme_view(target_module, target_def),
                    Some(crate::check::SchemeView::Query { .. })
                )
            {
                return Ok(None);
            }
            let target_body = &input.module(target_module).def(target_def).body;
            let Some((target_typed, checked)) =
                context.lower(input, target_module, target_def, target_body)?
            else {
                return Ok(None);
            };
            let mut typed = TypedQuery::new();
            let child = copy_typed_graph(&target_typed, &mut typed)?;
            (child, checked, typed)
        } else {
            return Ok(None);
        };
    let id = typed
        .keep_row_with_payload(
            stage.node,
            QueryNodeKind::Where,
            child,
            origin,
            Some(predicate.clone()),
            None,
            None,
            None,
        )
        .map_err(query_error)?;
    let checked = CheckedQuery::where_(checked, predicate, origin)
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
    Ok(Some((typed.finish(id), checked)))
}

fn lower_plain_expr(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
) -> Result<Option<crate::checked::CheckedExpr>, Error> {
    lower_plain_expr_with_row(input, module, expr, None)
}

fn lower_plain_expr_with_row(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
    row: Option<&RowType>,
) -> Result<Option<crate::checked::CheckedExpr>, Error> {
    if let Some(plain) = lower_plain_expr_basic(input, module, expr, row)? {
        return Ok(Some(plain));
    }
    let mut env = HashMap::new();
    let value = lower_plain_value(input, module, expr, row, &mut env, 0)?;
    if value.is_none() && application_depth(expr) > 16 {
        return Err(Error::new("definition expands to more than 200000 nodes")
            .at(Origin::new(module, expr.span)));
    }
    Ok(match value {
        Some(PlainValue::Expr(value)) => Some(value),
        Some(PlainValue::Function { .. }) | None => None,
    })
}

fn application_depth(expr: &Expr) -> usize {
    match &expr.kind {
        ExprKind::App(head, args) => {
            1 + application_depth(head).max(args.iter().map(application_depth).max().unwrap_or(0))
        }
        _ => crate::check::child_exprs(expr)
            .into_iter()
            .map(application_depth)
            .max()
            .unwrap_or(0),
    }
}

/// A checker-resolved scalar value.  The direct expression lowering above is
/// deliberately small; this value layer handles the function values that can
/// appear in scalar position (aliases, lambdas, and composition).
#[derive(Clone)]
enum PlainValue<'a> {
    Expr(crate::checked::CheckedExpr),
    Function {
        params: Vec<String>,
        body: PlainFunctionBody<'a>,
        module: usize,
        env: HashMap<String, PlainValue<'a>>,
    },
}

#[derive(Clone)]
enum PlainFunctionBody<'a> {
    Expr(&'a Expr),
    Template {
        sql: String,
        kind: crate::normalize::TemplateKind,
        ty: crate::core::ScalarType,
    },
}

fn scalar_operator_template(name: &str) -> Option<&'static str> {
    match name {
        "_+_" => Some("$1 + $2"),
        "_-_" => Some("$1 - $2"),
        "_*_" => Some("$1 * $2"),
        "_/_" => Some("$1 / $2"),
        "_%_" => Some("$1 % $2"),
        "_>_" => Some("$1 > $2"),
        "_<_" => Some("$1 < $2"),
        "_>=_" => Some("$1 >= $2"),
        "_<=_" => Some("$1 <= $2"),
        "_==_" => Some("$1 = $2"),
        "_!=_" => Some("$1 <> $2"),
        "_&&_" => Some("$1 AND $2"),
        "_||_" => Some("$1 OR $2"),
        _ => None,
    }
}

fn lower_plain_value<'a>(
    input: CompilerInput<'a>,
    module: usize,
    expr: &'a Expr,
    row: Option<&RowType>,
    env: &mut HashMap<String, PlainValue<'a>>,
    depth: usize,
) -> Result<Option<PlainValue<'a>>, Error> {
    if depth > 16 {
        return Ok(None);
    }
    if let Some(value) = lower_plain_expr_basic(input, module, expr, row)? {
        return Ok(Some(PlainValue::Expr(value)));
    }
    let tc = input.type_check();
    match &expr.kind {
        ExprKind::Name(name) => {
            if let Some(value) = env.get(name).cloned() {
                return Ok(Some(value));
            }
            if let Some(sql) = scalar_operator_template(name) {
                return Ok(Some(PlainValue::Function {
                    params: (0..2).map(|i| format!("$arg{i}")).collect(),
                    body: PlainFunctionBody::Template {
                        sql: sql.to_string(),
                        kind: crate::normalize::TemplateKind::Scalar,
                        ty: crate::core::ScalarType::Unknown,
                    },
                    module,
                    env: HashMap::new(),
                }));
            }
            let Some((target_module, target_def)) = scalar_definition_target(tc, module, expr.id)
            else {
                return Ok(None);
            };
            lower_plain_definition(input, target_module, target_def, row, depth + 1)
        }
        ExprKind::Proj(_, _) => {
            let Some((target_module, target_def)) = scalar_definition_target(tc, module, expr.id)
            else {
                return Ok(None);
            };
            lower_plain_definition(input, target_module, target_def, row, depth + 1)
        }
        ExprKind::Lambda(parameter, body) => Ok(Some(PlainValue::Function {
            params: vec![parameter.clone()],
            body: PlainFunctionBody::Expr(body),
            module,
            env: env.clone(),
        })),
        ExprKind::App(head, args) => {
            let Some(mut value) = lower_plain_value(input, module, head, row, env, depth + 1)?
            else {
                return Ok(None);
            };
            for arg in args.iter() {
                let Some(arg) = lower_plain_value(input, module, arg, row, env, depth + 1)? else {
                    return Ok(None);
                };
                let Some(applied) = apply_plain_value(input, value, arg, expr, row, depth + 1)?
                else {
                    return Ok(None);
                };
                value = applied;
            }
            Ok(Some(value))
        }
        _ => Ok(None),
    }
}

fn scalar_definition_target(
    tc: &crate::check::TypeCheck,
    module: usize,
    expr_id: u32,
) -> Option<(usize, usize)> {
    if let Some(Resolved::Def(target)) = tc.resolution(module, expr_id) {
        return Some(*target);
    }
    let mut choices = tc
        .typed_node(module, expr_id)?
        .choices
        .iter()
        .filter_map(|(_, choice)| match choice {
            crate::check::Choice::Def(module, def) => Some((*module, *def)),
            crate::check::Choice::Hole(_) => None,
        });
    let target = choices.next()?;
    choices
        .all(|candidate| candidate == target)
        .then_some(target)
}

fn lower_plain_definition<'a>(
    input: CompilerInput<'a>,
    module: usize,
    def: usize,
    row: Option<&RowType>,
    depth: usize,
) -> Result<Option<PlainValue<'a>>, Error> {
    if depth > 16 {
        return Ok(None);
    }
    let declaration = input.module(module).def(def);
    match &declaration.body.kind {
        ExprKind::Sql(sql) => {
            let params = template_arity(declaration.ty.as_ref());
            let ty = input
                .type_check()
                .result_scalar(module, def)
                .unwrap_or(crate::core::ScalarType::Unknown);
            Ok(Some(PlainValue::Function {
                params: (0..params).map(|i| format!("$arg{i}")).collect(),
                body: PlainFunctionBody::Template {
                    sql: sql.clone(),
                    kind: crate::normalize::declared_template_kind(declaration),
                    ty,
                },
                module,
                env: HashMap::new(),
            }))
        }
        ExprKind::Lambda(parameter, body) => Ok(Some(PlainValue::Function {
            params: vec![parameter.clone()],
            body: PlainFunctionBody::Expr(body),
            module,
            env: HashMap::new(),
        })),
        ExprKind::Name(_) | ExprKind::Proj(_, _) => {
            let Some(Resolved::Def((target_module, target_def))) =
                input.type_check().resolution(module, declaration.body.id)
            else {
                return Ok(None);
            };
            lower_plain_definition(input, *target_module, *target_def, row, depth + 1)
        }
        _ => {
            let mut env = HashMap::new();
            lower_plain_value(input, module, &declaration.body, row, &mut env, depth + 1)
        }
    }
}

fn template_arity(ty: Option<&cagara_syntax::ast::TypeExpr>) -> usize {
    let mut count = 0;
    let mut current = ty;
    while let Some(cagara_syntax::ast::TypeExpr::Fun(_, next)) = current {
        count += 1;
        current = Some(next.as_ref());
    }
    count
}

fn apply_plain_value<'a>(
    input: CompilerInput<'a>,
    value: PlainValue<'a>,
    arg: PlainValue<'a>,
    call: &Expr,
    row: Option<&RowType>,
    depth: usize,
) -> Result<Option<PlainValue<'a>>, Error> {
    let PlainValue::Function {
        mut params,
        body,
        module,
        mut env,
    } = value
    else {
        return Ok(None);
    };
    let Some(parameter) = params.first().cloned() else {
        return Ok(None);
    };
    params.remove(0);
    env.insert(parameter, arg);
    if !params.is_empty() {
        return Ok(Some(PlainValue::Function {
            params,
            body,
            module,
            env,
        }));
    }
    match body {
        PlainFunctionBody::Expr(body) => {
            lower_plain_value(input, module, body, row, &mut env, depth + 1)
        }
        PlainFunctionBody::Template { sql, kind, ty } => {
            let mut bindings: Vec<_> = env.into_iter().collect();
            bindings.sort_by_key(|(name, _)| {
                name.strip_prefix("$arg")
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(usize::MAX)
            });
            let args = bindings
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>();
            let mut checked = Vec::with_capacity(args.len());
            for value in args {
                let PlainValue::Expr(value) = value else {
                    return Ok(None);
                };
                checked.push(value);
            }
            let ty = if matches!(&ty, crate::core::ScalarType::Unknown) {
                input
                    .type_check()
                    .typed_node(module, call.id)
                    .and_then(|node| node.ty())
                    .unwrap_or(ty)
            } else {
                ty
            };
            let origin = Origin::new(module, call.span);
            let checked = match kind {
                crate::normalize::TemplateKind::Scalar => {
                    crate::checked::CheckedExpr::template(sql, checked, ty, origin)
                }
                crate::normalize::TemplateKind::Agg => {
                    crate::checked::CheckedExpr::agg_template(sql, checked, ty, origin)
                }
                crate::normalize::TemplateKind::Win => return Ok(None),
            }
            .map_err(|error| Error::unsupported(error.message).at(origin))?;
            Ok(Some(PlainValue::Expr(checked)))
        }
    }
}

/// Lower the direct source expression subset. Expressions that need a
/// checker-resolved function value are handled by [`lower_plain_value`] after
/// this fast path declines them.
fn lower_plain_expr_basic(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
    row: Option<&RowType>,
) -> Result<Option<crate::checked::CheckedExpr>, Error> {
    let tc = input.type_check();
    let origin = Origin::new(module, expr.span);
    match &expr.kind {
        ExprKind::Field(side, column) => {
            let ty = row
                .filter(|_| *side == cagara_syntax::ast::Side::Single)
                .and_then(|row| row.get(column).cloned())
                .or_else(|| tc.typed_node(module, expr.id).and_then(|node| node.ty()));
            let Some(ty) = ty else {
                return Ok(None);
            };
            Ok(Some(crate::checked::CheckedExpr::column(
                *side,
                column.clone(),
                ty,
                origin,
            )))
        }
        ExprKind::Lit(lit) => Ok(Some(crate::checked::CheckedExpr::lit(
            match lit {
                Lit::Int(value) => crate::ir::Lit::Int(*value),
                Lit::Float(value) => crate::ir::Lit::Float(value.clone()),
                Lit::Str(value) => crate::ir::Lit::Str(value.clone()),
                Lit::Bool(value) => crate::ir::Lit::Bool(*value),
            },
            origin,
        ))),
        ExprKind::Case(arms) => {
            let Some(((default_condition, default_value), when_arms)) = arms.split_last() else {
                return Ok(None);
            };
            if !matches!(default_condition.kind, ExprKind::Lit(Lit::Bool(true))) {
                return Ok(None);
            }
            if when_arms.is_empty() {
                return lower_plain_expr_with_row(input, module, default_value, row);
            }
            let mut args = Vec::with_capacity(when_arms.len() * 2 + 1);
            let mut sql = String::from("CASE");
            for (condition, value) in when_arms {
                let Some(condition) = lower_plain_expr_with_row(input, module, condition, row)?
                else {
                    return Ok(None);
                };
                let Some(value) = lower_plain_expr_with_row(input, module, value, row)? else {
                    return Ok(None);
                };
                let condition_index = args.len() + 1;
                args.push(condition);
                let value_index = args.len() + 1;
                args.push(value);
                sql.push_str(&format!(" WHEN ${condition_index} THEN ${value_index}"));
            }
            let Some(default) = lower_plain_expr_with_row(input, module, default_value, row)?
            else {
                return Ok(None);
            };
            let default_index = args.len() + 1;
            args.push(default);
            sql.push_str(&format!(" ELSE ${default_index} END"));
            let Some(ty) = tc.typed_node(module, expr.id).and_then(|node| node.ty()) else {
                return Ok(None);
            };
            crate::checked::CheckedExpr::template(sql, args, ty, origin)
                .map(Some)
                .map_err(|error| Error::unsupported(error.message).at(origin))
        }
        ExprKind::Name(_) => {
            let Some((sql, kind, arity)) = declared_template(input, module, expr) else {
                return Ok(None);
            };
            if arity != 0 {
                return Ok(None);
            }
            let Some(ty) = tc.typed_node(module, expr.id).and_then(|node| node.ty()) else {
                return Ok(None);
            };
            match kind {
                crate::normalize::TemplateKind::Scalar => {
                    crate::checked::CheckedExpr::template(sql, Vec::new(), ty, origin)
                }
                crate::normalize::TemplateKind::Agg => {
                    crate::checked::CheckedExpr::agg_template(sql, Vec::new(), ty, origin)
                }
                crate::normalize::TemplateKind::Win => return Ok(None),
            }
            .map(Some)
            .map_err(|error| Error::unsupported(error.message).at(origin))
        }
        ExprKind::App(_head, args) => {
            // An overloaded scalar operator inside a polymorphic helper has a
            // checker hole at its definition node, so there is no selected
            // template definition to read there. Its use-site arguments are
            // concrete after helper instantiation; preserve that checked
            // payload directly for the small operator family represented by
            // the core AST.
            if let ExprKind::Name(operator) = &_head.kind {
                let sql = match operator.as_str() {
                    "_+_" => Some("$1 + $2"),
                    "_-_" => Some("$1 - $2"),
                    "_*_" => Some("$1 * $2"),
                    "_/_" => Some("$1 / $2"),
                    "_%_" => Some("$1 % $2"),
                    "_>_" => Some("$1 > $2"),
                    "_<_" => Some("$1 < $2"),
                    "_>=_" => Some("$1 >= $2"),
                    "_<=_" => Some("$1 <= $2"),
                    "_==_" => Some("$1 = $2"),
                    "_!=_" => Some("$1 <> $2"),
                    "_&&_" => Some("$1 AND $2"),
                    "_||_" => Some("$1 OR $2"),
                    _ => None,
                };
                if let Some(sql) = sql {
                    let mut lowered = Vec::with_capacity(args.len());
                    for arg in args.iter() {
                        let Some(arg) = lower_plain_expr_with_row(input, module, arg, row)? else {
                            return Ok(None);
                        };
                        lowered.push(arg);
                    }
                    let ty = if operator == "_>_"
                        || operator == "_<_"
                        || operator == "_>=_"
                        || operator == "_<=_"
                        || operator == "_==_"
                        || operator == "_!=_"
                        || operator == "_&&_"
                        || operator == "_||_"
                    {
                        crate::core::ScalarType::Bool
                    } else {
                        lowered
                            .first()
                            .map(|arg| arg.ty().clone())
                            .unwrap_or(crate::core::ScalarType::Unknown)
                    };
                    return crate::checked::CheckedExpr::template(sql, lowered, ty, origin)
                        .map(Some)
                        .map_err(|error| Error::unsupported(error.message).at(origin));
                }
            }
            if let Some((sql, kind, arity)) = declared_template(input, module, expr) {
                if args.len() != arity {
                    return Ok(None);
                }
                if kind == crate::normalize::TemplateKind::Win {
                    let Some((spec_expr, values)) = args.split_first() else {
                        return Ok(None);
                    };
                    let Some(spec) = lower_plain_window_spec(input, module, spec_expr, row, 0)?
                    else {
                        return Ok(None);
                    };
                    let mut checked_args = Vec::with_capacity(values.len());
                    for value in values {
                        let Some(value) = lower_plain_expr_with_row(input, module, value, row)?
                        else {
                            return Ok(None);
                        };
                        checked_args.push(value);
                    }
                    let Some(ty) = tc.typed_node(module, expr.id).and_then(|node| node.ty()) else {
                        return Ok(None);
                    };
                    return crate::checked::CheckedExpr::win_template(
                        sql,
                        checked_args,
                        spec,
                        ty,
                        origin,
                    )
                    .map(Some)
                    .map_err(|error| Error::unsupported(error.message).at(origin));
                }
                let mut checked_args = Vec::with_capacity(args.len());
                for arg in args {
                    let Some(arg) = lower_plain_expr_with_row(input, module, arg, row)? else {
                        return Ok(None);
                    };
                    checked_args.push(arg);
                }
                let Some(ty) = tc.typed_node(module, expr.id).and_then(|node| node.ty()) else {
                    return Ok(None);
                };
                return match kind {
                    crate::normalize::TemplateKind::Scalar => {
                        crate::checked::CheckedExpr::template(sql, checked_args, ty, origin)
                    }
                    crate::normalize::TemplateKind::Agg => {
                        crate::checked::CheckedExpr::agg_template(sql, checked_args, ty, origin)
                    }
                    crate::normalize::TemplateKind::Win => unreachable!(),
                }
                .map(Some)
                .map_err(|error| Error::unsupported(error.message).at(origin));
            }
            let Some((primitive, primitive_args)) = declared_primitive(input, module, expr) else {
                return Ok(None);
            };
            if primitive == "__group" {
                if primitive_args.len() != 1 {
                    return Ok(None);
                }
                let Some(key) = lower_plain_expr_with_row(input, module, &primitive_args[0], row)?
                else {
                    return Ok(None);
                };
                return crate::checked::CheckedExpr::group(key, origin)
                    .map(Some)
                    .map_err(|error| Error::unsupported(error.message).at(origin));
            }
            if primitive == "__in" {
                if primitive_args.len() != 2 {
                    return Ok(None);
                }
                let ExprKind::List(items) = &primitive_args[0].kind else {
                    return Ok(None);
                };
                let Some(value) =
                    lower_plain_expr_with_row(input, module, &primitive_args[1], row)?
                else {
                    return Ok(None);
                };
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let Some(item) = lower_plain_expr_with_row(input, module, item, row)? else {
                        return Ok(None);
                    };
                    list.push(item);
                }
                return crate::checked::CheckedExpr::in_(value, list, false, origin)
                    .map(Some)
                    .map_err(|error| Error::unsupported(error.message).at(origin));
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Resolve a SQL template from the selected definition. The declaration's
/// body and declared result kind are authoritative, so editable prelude names
/// and aliases do not need a Rust dispatch table.
fn declared_template(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
) -> Option<(String, crate::normalize::TemplateKind, usize)> {
    let ((target_module, target_def), args) = helper_application(input.type_check(), module, expr)?;
    let (sql, kind) = template_definition(input, target_module, target_def, 0)?;
    Some((sql, kind, args.len()))
}

fn template_definition(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    depth: usize,
) -> Option<(String, crate::normalize::TemplateKind)> {
    if depth > 32 {
        return None;
    }
    let declaration = input.module(module).def(def);
    if let ExprKind::Sql(sql) = &declaration.body.kind {
        return Some((
            sql.clone(),
            crate::normalize::declared_template_kind(declaration),
        ));
    }
    let target = match &declaration.body.kind {
        ExprKind::Name(_) | ExprKind::Proj(_, _) => {
            input.type_check().resolution(module, declaration.body.id)
        }
        _ => None,
    }?;
    let Resolved::Def((target_module, target_def)) = target else {
        return None;
    };
    template_definition(input, *target_module, *target_def, depth + 1)
}

/// Resolve a bare name to the checker-selected SQL template, including an
/// overload choice recorded at that use site.
fn resolved_template_definition(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
) -> Option<(usize, usize, String, crate::normalize::TemplateKind)> {
    let tc = input.type_check();
    let (target_module, target_def) = match tc.resolution(module, expr.id)? {
        Resolved::Def((target_module, target_def)) => (*target_module, *target_def),
        Resolved::Overloads {
            module: overload_module,
            defs,
        } => {
            let choice = tc.typed_node(module, expr.id)?.choices.iter().find_map(
                |(_, choice)| match choice {
                    crate::check::Choice::Def(_, def) if defs.contains(def) => {
                        Some((*overload_module, *def))
                    }
                    _ => None,
                },
            )?;
            choice
        }
        Resolved::Primitive => return None,
    };
    let (sql, kind) = template_definition(input, target_module, target_def, 0)?;
    Some((target_module, target_def, sql, kind))
}

/// Resolve a primitive declaration, following ordinary aliases. Primitive
/// identity is metadata (`__group`, `__in`), not a prelude name.
fn declared_primitive(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
) -> Option<(String, Vec<Expr>)> {
    let ((target_module, target_def), args) = helper_application(input.type_check(), module, expr)?;
    let primitive = primitive_definition(input, target_module, target_def, 0)?;
    Some((primitive, args))
}

/// Resolve a sort-key constructor through its primitive definition. The
/// spelling (`asc`, `desc`, or an alias) belongs to the prelude; typed lowering
/// only needs the primitive identity and therefore follows the selected
/// definition metadata.
fn sort_direction(input: CompilerInput<'_>, module: usize, expr: &Expr) -> Option<bool> {
    let (primitive, args) = declared_primitive(input, module, expr)?;
    if args.len() != 1 {
        return None;
    }
    match primitive.as_str() {
        "__asc" => Some(true),
        "__desc" => Some(false),
        _ => None,
    }
}

fn primitive_definition(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    depth: usize,
) -> Option<String> {
    if depth > 32 {
        return None;
    }
    let declaration = input.module(module).def(def);
    if let ExprKind::Primitive(name) = &declaration.body.kind {
        return Some(name.clone());
    }
    let target = match &declaration.body.kind {
        ExprKind::Name(_) | ExprKind::Proj(_, _) => {
            input.type_check().resolution(module, declaration.body.id)
        }
        _ => None,
    }?;
    let Resolved::Def((target_module, target_def)) = target else {
        return None;
    };
    primitive_definition(input, *target_module, *target_def, depth + 1)
}

/// Build the plain window specification directly from source, following named
/// specs and frame aliases through their resolved definitions.
fn lower_plain_window_spec(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
    row: Option<&RowType>,
    depth: usize,
) -> Result<Option<crate::checked::WinSpecChecked>, Error> {
    if depth > 32 {
        return Ok(None);
    }
    if let ExprKind::Name(_) | ExprKind::Proj(_, _) = expr.kind {
        if let Some(Resolved::Def((target_module, target_def))) =
            input.type_check().resolution(module, expr.id)
        {
            let target = &input.module(*target_module).def(*target_def).body;
            if !matches!(target.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
                return lower_plain_window_spec(input, *target_module, target, row, depth + 1);
            }
        }
        return Ok(None);
    }
    let ExprKind::Record(fields) = &expr.kind else {
        return Ok(None);
    };
    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    for (key, value) in fields {
        match key.as_str() {
            crate::rules::winspec::PARTITION => {
                let ExprKind::List(items) = &value.kind else {
                    return Ok(None);
                };
                for item in items {
                    let Some(item) = lower_plain_expr_with_row(input, module, item, row)? else {
                        return Ok(None);
                    };
                    partition.push(item);
                }
            }
            crate::rules::winspec::ORDER => {
                let ExprKind::List(items) = &value.kind else {
                    return Ok(None);
                };
                for item in items {
                    let (value, ascending) = match &item.kind {
                        ExprKind::App(_, args) if args.len() == 1 => {
                            let Some(ascending) = sort_direction(input, module, item) else {
                                return Ok(None);
                            };
                            (&args[0], ascending)
                        }
                        _ => (item, true),
                    };
                    let Some(value) = lower_plain_expr_with_row(input, module, value, row)? else {
                        return Ok(None);
                    };
                    order.push((value, ascending));
                }
            }
            crate::rules::winspec::FRAME => {
                let Some(lowered) = lower_plain_frame(input, module, value, depth + 1)? else {
                    return Ok(None);
                };
                frame = Some(lowered);
            }
            _ => return Ok(None),
        }
    }
    crate::checked::window_spec(partition, order, frame)
        .map(Some)
        .map_err(|error| Error::unsupported(error.message).at(Origin::new(module, expr.span)))
}

fn lower_plain_frame(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
    depth: usize,
) -> Result<Option<crate::ir::Frame>, Error> {
    if depth > 32 {
        return Ok(None);
    }
    if matches!(expr.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
        if let Some(Resolved::Def((target_module, target_def))) =
            input.type_check().resolution(module, expr.id)
        {
            let target = &input.module(*target_module).def(*target_def).body;
            if !matches!(target.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
                return lower_plain_frame(input, *target_module, target, depth + 1);
            }
        }
        return Ok(None);
    }
    let resolved = match &expr.kind {
        ExprKind::Primitive(primitive) => Some((primitive.clone(), Vec::new())),
        _ => declared_primitive(input, module, expr),
    };
    let Some((primitive, args)) = resolved else {
        return Ok(None);
    };
    if primitive != "__rows" || args.len() != 2 {
        return Ok(None);
    }
    let Some(start) = lower_plain_bound(input, module, &args[0], depth + 1)? else {
        return Ok(None);
    };
    let Some(end) = lower_plain_bound(input, module, &args[1], depth + 1)? else {
        return Ok(None);
    };
    match crate::checked::frame(start, end) {
        Ok(frame) => Ok(Some(frame)),
        // Leave invalid or unsupported frame forms to the established route,
        // which owns the user-facing diagnostic.
        Err(error) => Err(Error::unsupported(error.message).at(Origin::new(module, expr.span))),
    }
}

fn lower_plain_bound(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
    depth: usize,
) -> Result<Option<crate::ir::Bound>, Error> {
    if depth > 32 {
        return Ok(None);
    }
    if matches!(expr.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
        if let Some(Resolved::Def((target_module, target_def))) =
            input.type_check().resolution(module, expr.id)
        {
            let target = &input.module(*target_module).def(*target_def).body;
            if !matches!(target.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
                return lower_plain_bound(input, *target_module, target, depth + 1);
            }
        }
    }
    let resolved = match &expr.kind {
        ExprKind::Primitive(primitive) => Some((primitive.clone(), Vec::new())),
        _ => declared_primitive(input, module, expr),
    };
    let Some((primitive, args)) = resolved else {
        return Ok(None);
    };
    let bound = match (primitive.as_str(), args.as_slice()) {
        ("__unboundedPreceding", []) => crate::ir::Bound::UnboundedPreceding,
        ("__unboundedFollowing", []) => crate::ir::Bound::UnboundedFollowing,
        ("__currentRow", []) => crate::ir::Bound::CurrentRow,
        (
            "__preceding",
            [Expr {
                kind: ExprKind::Lit(Lit::Int(value)),
                ..
            }],
        ) => crate::ir::Bound::Preceding(*value),
        (
            "__following",
            [Expr {
                kind: ExprKind::Lit(Lit::Int(value)),
                ..
            }],
        ) => crate::ir::Bound::Following(*value),
        _ => return Ok(None),
    };
    Ok(Some(bound))
}

/// Lower a direct order whose keys fit the source checked-expression subset.
fn lower_simple_order_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.len() != 1
        || stages[0].kind != RowStage::Order
        || stages[0].node.definition_module != module
        || stages[0].node.definition != def
        || !is_direct_pipeline(input, module, body)
        || contains_other_stage(input, module, body, RowStage::Order)
    {
        return Ok(None);
    }
    let stage = &stages[0];
    let stage_sites = std::iter::once(stage.site).collect::<HashSet<_>>();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let stage_expr = find_expr_id(body, stage.site).unwrap_or(body);
    let ExprKind::App(_, args) = &stage_expr.kind else {
        return Ok(None);
    };
    let Some(list) = args.first() else {
        return Ok(None);
    };
    let ExprKind::List(items) = &list.kind else {
        return Ok(None);
    };
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let ExprKind::App(_, args) = &item.kind else {
            return Ok(None);
        };
        let Some(value) = args.first() else {
            return Ok(None);
        };
        let Some(direction) = sort_direction(input, module, item) else {
            return Ok(None);
        };
        let Some(value) = lower_plain_expr(input, module, value)? else {
            return Ok(None);
        };
        keys.push((value, direction));
    }
    let origin = Origin::new(module, stage_expr.span);
    let (child, checked, mut typed) =
        if let Some((schema, table, columns)) = table_parts(input, module, base) {
            let checked =
                CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                    .map_err(|error| Error::unsupported(error.message).at(origin))?;
            let mut typed = TypedQuery::new();
            let child = typed
                .table(NodeRef::new(module, def, base.id), origin, columns)
                .map_err(query_error)?;
            (child, checked, typed)
        } else if let Some((target_module, target_def)) = query_target(tc, module, base) {
            if (target_module, target_def) == (module, def)
                || !matches!(
                    tc.scheme_view(target_module, target_def),
                    Some(crate::check::SchemeView::Query { .. })
                )
            {
                return Ok(None);
            }
            let target_body = &input.module(target_module).def(target_def).body;
            let Some((target_typed, checked)) =
                context.lower(input, target_module, target_def, target_body)?
            else {
                return Ok(None);
            };
            let mut typed = TypedQuery::new();
            let child = copy_typed_graph(&target_typed, &mut typed)?;
            (child, checked, typed)
        } else {
            return Ok(None);
        };
    let id = typed
        .keep_row_with_payload(
            stage.node,
            QueryNodeKind::Order,
            child,
            origin,
            None,
            Some(keys.clone()),
            None,
            None,
        )
        .map_err(query_error)?;
    let checked = CheckedQuery::order(checked, keys, origin)
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
    Ok(Some((typed.finish(id), checked)))
}

/// Lower a complete direct pipeline when every expression payload fits the
/// plain checked-expression subset. This extends the single-stage slices to
/// mixed chains, using each recorded input row to type fields after earlier
/// projections and rewrites without consulting normalized terms.
fn lower_plain_expression_chain(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.is_empty()
        || stages.iter().any(|stage| {
            stage.node.definition_module != module
                || stage.node.definition != def
                || stage.input.is_none()
                || stage.output.is_none()
        })
        || !is_direct_pipeline(input, module, body)
    {
        return Ok(None);
    }
    let stage_sites: HashSet<_> = stages.iter().map(|stage| stage.site).collect();
    let had_distinct_record = stages.iter().any(|stage| stage.kind == RowStage::Distinct);
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let mut syntax = HashMap::new();
    index_exprs(body, &mut syntax);
    let (mut child, mut checked, mut typed) =
        if let Some((schema, table, columns)) = table_parts(input, module, base) {
            let origin = table_origin(input, module, base);
            let checked =
                CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                    .map_err(|error| Error::unsupported(error.message).at(origin))?;
            let mut typed = TypedQuery::new();
            let child = typed
                .table(NodeRef::new(module, def, base.id), origin, columns)
                .map_err(query_error)?;
            (child, checked, typed)
        } else if let Some((target_module, target_def)) = query_target(tc, module, base) {
            if (target_module, target_def) == (module, def)
                || !matches!(
                    tc.scheme_view(target_module, target_def),
                    Some(crate::check::SchemeView::Query { .. })
                )
            {
                return Ok(None);
            }
            let target_body = &input.module(target_module).def(target_def).body;
            let Some((target_typed, checked)) =
                context.lower(input, target_module, target_def, target_body)?
            else {
                return Ok(None);
            };
            let mut typed = TypedQuery::new();
            let child = copy_typed_graph(&target_typed, &mut typed)?;
            (child, checked, typed)
        } else {
            return Ok(None);
        };

    for stage in stages {
        let Some(stage_expr) = syntax.get(&stage.site).copied() else {
            return Ok(None);
        };
        let ExprKind::App(_, args) = &stage_expr.kind else {
            return Ok(None);
        };
        let origin = Origin::new(stage.node.definition_module, stage_expr.span);
        let checked_origin = origin;
        let input_row = Row::new(stage.input.clone().expect("checked above"));
        let output_row = Row::new(stage.output.clone().expect("checked above"));
        let expression_row = RowType::new(input_row.columns().to_vec());
        let mut predicate = None;
        let mut keys = None;
        let mut fields = None;
        let mut label = None;
        let mut count = None;

        match stage.kind {
            RowStage::Where => {
                let Some(expr) = args.first() else {
                    return Ok(None);
                };
                predicate = lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?;
                if predicate.is_none() {
                    return Ok(None);
                }
            }
            RowStage::Select | RowStage::Update | RowStage::Agg => {
                let Some(record) = args.first() else {
                    return Ok(None);
                };
                let ExprKind::Record(source_fields) = &record.kind else {
                    return Ok(None);
                };
                let mut lowered = Vec::with_capacity(source_fields.len());
                for (name, expr) in source_fields {
                    let Some(expr) =
                        lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?
                    else {
                        return Ok(None);
                    };
                    lowered.push((name.clone(), expr));
                }
                fields = Some(lowered);
            }
            RowStage::Order => {
                let Some(list) = args.first() else {
                    return Ok(None);
                };
                let ExprKind::List(items) = &list.kind else {
                    return Ok(None);
                };
                let mut lowered = Vec::with_capacity(items.len());
                for item in items {
                    let ExprKind::App(_, item_args) = &item.kind else {
                        return Ok(None);
                    };
                    let Some(expr) = item_args.first() else {
                        return Ok(None);
                    };
                    let Some(direction) = sort_direction(input, module, item) else {
                        return Ok(None);
                    };
                    let Some(expr) =
                        lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?
                    else {
                        return Ok(None);
                    };
                    lowered.push((expr, direction));
                }
                keys = Some(lowered);
            }
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
                label = stage.label.clone();
                if label.is_none() {
                    return Ok(None);
                }
            }
            RowStage::Limit | RowStage::Offset => {
                count = stage_count(&stage_expr);
                if count.is_none() {
                    let argument = args.first().map(|arg| arg.span).unwrap_or(stage_expr.span);
                    return Err(
                        Error::new("limit expects an int").at(Origin::new(module, argument))
                    );
                }
            }
            RowStage::Distinct => {}
        }

        let kind = match stage.kind {
            RowStage::Prefix => QueryNodeKind::Prefix,
            RowStage::Suffix => QueryNodeKind::Suffix,
            RowStage::Omit => QueryNodeKind::Omit,
            RowStage::MapValue => QueryNodeKind::MapValue,
            RowStage::Where => QueryNodeKind::Where,
            RowStage::Select => QueryNodeKind::Select,
            RowStage::Update => QueryNodeKind::Update,
            RowStage::Agg => QueryNodeKind::Agg,
            RowStage::Order => QueryNodeKind::Order,
            RowStage::Limit => QueryNodeKind::Limit,
            RowStage::Offset => QueryNodeKind::Offset,
            RowStage::Distinct => QueryNodeKind::Distinct,
        };
        let id = match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => typed
                .rewrite(
                    stage.node,
                    kind,
                    child,
                    Some(input_row.clone()),
                    Some(output_row.clone()),
                    origin,
                )
                .map_err(query_error)?,
            RowStage::Select | RowStage::Update | RowStage::Agg => typed
                .project(
                    stage.node,
                    kind,
                    child,
                    Some(output_row.clone()),
                    origin,
                    fields.clone(),
                )
                .map_err(query_error)?,
            _ => typed
                .keep_row_with_payload(
                    stage.node,
                    kind,
                    child,
                    origin,
                    predicate.clone(),
                    keys.clone(),
                    None,
                    None,
                )
                .map_err(query_error)?,
        };
        checked = match stage.kind {
            RowStage::Prefix => {
                CheckedQuery::prefix(checked, label.expect("label"), checked_origin)
            }
            RowStage::Suffix => {
                CheckedQuery::suffix(checked, label.expect("label"), checked_origin)
            }
            RowStage::Omit => CheckedQuery::omit(checked, label.expect("label"), checked_origin),
            RowStage::MapValue => {
                CheckedQuery::map_value(checked, label.expect("label"), checked_origin)
            }
            RowStage::Where => {
                CheckedQuery::where_(checked, predicate.expect("predicate"), checked_origin)
            }
            RowStage::Select => {
                CheckedQuery::select(checked, fields.expect("fields"), checked_origin)
            }
            RowStage::Update => {
                CheckedQuery::update(checked, fields.expect("fields"), checked_origin)
            }
            RowStage::Agg => CheckedQuery::agg(checked, fields.expect("fields"), checked_origin),
            RowStage::Order => CheckedQuery::order(checked, keys.expect("keys"), checked_origin),
            RowStage::Limit => CheckedQuery::limit(checked, count.expect("count"), checked_origin),
            RowStage::Offset => {
                CheckedQuery::offset(checked, count.expect("count"), checked_origin)
            }
            RowStage::Distinct => CheckedQuery::distinct(checked, checked_origin),
        }
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let node = typed.node(id);
        if !rows_match(
            stage.kind,
            stage.input.as_deref(),
            node.input_row().map(Row::columns),
        ) || !rows_match(
            stage.kind,
            stage.output.as_deref(),
            node.output_row().map(Row::columns),
        ) {
            return Err(Error::unsupported(format!(
                "typed stage {:?} produced rows different from its checked record",
                stage.kind
            ))
            .at(origin));
        }
        child = id;
    }
    // `distinct` has no row-stage record because it carries no payload. A
    // direct pipeline can therefore reach this chain with the projection
    // recorded as its only stage; preserve a trailing distinct explicitly.
    if !had_distinct_record {
        if let Some(outer) = outer_pipeline_stage(body) {
            if stage_kind(input, module, outer) == Some(RowStage::Distinct) {
                let origin = Origin::new(module, outer.span);
                let id = typed
                    .keep_row(
                        NodeRef::new(module, def, outer.id),
                        QueryNodeKind::Distinct,
                        child,
                        origin,
                    )
                    .map_err(query_error)?;
                let checked = CheckedQuery::distinct(checked, origin)
                    .map_err(|error| Error::unsupported(error.message).at(origin))?;
                return Ok(Some((typed.finish(id), checked)));
            }
        }
    }
    Ok(Some((typed.finish(child), checked)))
}

fn contains_distinct_stage(input: CompilerInput<'_>, module: usize, body: &Expr) -> bool {
    let tc = input.type_check();
    let mut stack = vec![body];
    while let Some(expr) = stack.pop() {
        if matches!(&expr.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
            if let Some(Resolved::Def((target_module, target_def))) = tc.resolution(module, expr.id)
            {
                if let ExprKind::Primitive(name) =
                    &input.module(*target_module).def(*target_def).body.kind
                {
                    if crate::primitive::prim_of(name)
                        .and_then(RowStage::of_prim)
                        .is_some_and(|stage| stage == RowStage::Distinct)
                    {
                        return true;
                    }
                }
            }
        }
        stack.extend(crate::check::child_exprs(expr));
    }
    false
}

fn find_pipeline_stage<'a>(
    input: CompilerInput<'_>,
    module: usize,
    expr: &'a Expr,
    wanted: RowStage,
) -> Option<&'a Expr> {
    if let ExprKind::App(head, args) = &expr.kind {
        if args.len() == 2 && is_pipe_head(head) {
            let rhs = &args[1];
            if stage_kind(input, module, rhs) == Some(wanted) {
                return Some(rhs);
            }
            if let Some(found) = find_pipeline_stage(input, module, &args[0], wanted) {
                return Some(found);
            }
        }
    }
    crate::check::child_exprs(expr)
        .into_iter()
        .find_map(|child| find_pipeline_stage(input, module, child, wanted))
}

fn outer_pipeline_stage<'a>(expr: &'a Expr) -> Option<&'a Expr> {
    let ExprKind::App(head, args) = &expr.kind else {
        return None;
    };
    (args.len() == 2 && is_pipe_head(head)).then(|| &args[1])
}

fn stage_kind(input: CompilerInput<'_>, module: usize, expr: &Expr) -> Option<RowStage> {
    let head = match &expr.kind {
        ExprKind::App(head, _) => head.as_ref(),
        ExprKind::Name(_) | ExprKind::Proj(_, _) => expr,
        _ => return None,
    };
    let Resolved::Def((stage_module, stage_def)) =
        input.type_check().resolution(module, head.id)?
    else {
        return None;
    };
    let ExprKind::Primitive(name) = &input.module(*stage_module).def(*stage_def).body.kind else {
        return None;
    };
    crate::primitive::prim_of(name).and_then(RowStage::of_prim)
}

fn contains_other_stage(
    input: CompilerInput<'_>,
    module: usize,
    body: &Expr,
    allowed: RowStage,
) -> bool {
    let tc = input.type_check();
    let mut stack = vec![body];
    while let Some(expr) = stack.pop() {
        if matches!(&expr.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
            if let Some(Resolved::Def((stage_module, stage_def))) = tc.resolution(module, expr.id) {
                if let ExprKind::Primitive(name) =
                    &input.module(*stage_module).def(*stage_def).body.kind
                {
                    if let Some(prim) = crate::primitive::prim_of(name) {
                        if let Some(stage) = RowStage::of_prim(prim) {
                            if stage != allowed {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        stack.extend(crate::check::child_exprs(expr));
    }
    false
}

/// Lower a direct join or set operation whose two relational operands are
/// tables or already typed query aliases. The checker record supplies the
/// operation kind and both closed operand rows; the checked query supplies a
/// join predicate when this operation is a join.
fn lower_combine_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    if !tc.row_stages(module, def).next().is_none() {
        return Ok(None);
    }
    let Some(combine) = tc
        .typed_body(module, def)
        .into_iter()
        .flat_map(|body| body.nodes().iter())
        .find_map(|node| node.combine.as_ref())
        .cloned()
    else {
        return Ok(None);
    };
    let direct_combine = tc
        .typed_node(module, body.id)
        .and_then(|node| node.combine.as_ref())
        .is_some();
    let (left_expr, operation) = if direct_combine {
        let ExprKind::App(_, args) = &body.kind else {
            return Ok(None);
        };
        let Some(left) = args.first() else {
            return Ok(None);
        };
        (left, body)
    } else {
        let Some(context) = find_combine_context(body, combine.site) else {
            return Ok(None);
        };
        context
    };
    let ExprKind::App(_, args) = &operation.kind else {
        return Ok(None);
    };
    let Some(right_expr) = args.get(usize::from(direct_combine)) else {
        return Ok(None);
    };
    let mut typed = TypedQuery::new();
    let Some((left, left_checked)) =
        lower_combine_input(context, input, module, def, left_expr, &mut typed)?
    else {
        return Ok(None);
    };
    let Some((right, right_checked)) =
        lower_combine_input(context, input, module, def, right_expr, &mut typed)?
    else {
        return Ok(None);
    };
    let kind = match combine.kind {
        CombineKind::Join(kind) => QueryNodeKind::Join(kind),
        CombineKind::Set(kind) => QueryNodeKind::Set(kind),
    };
    // Set primitives declare their explicit operand before the piped input,
    // while the source spelling puts the piped input on the left. Preserve
    // the checked operation's operand order in both the typed graph and the
    // direct checked constructor.
    let (typed_left, typed_right, checked_left, checked_right) = match combine.kind {
        CombineKind::Join(_) => (left, right, left_checked, right_checked),
        // In a direct call (`union users vips`) the source arguments already
        // carry the operation's left-to-right order. The explicit swap is
        // only needed for the infix/piped spelling where the primitive's
        // declared argument precedes the piped input.
        CombineKind::Set(_) if direct_combine => (left, right, left_checked, right_checked),
        CombineKind::Set(_) => (right, left, right_checked, left_checked),
    };
    let origin = Origin::new(combine.node.definition_module, operation.span);
    let checked = match combine.kind {
        CombineKind::Set(kind) => CheckedQuery::set(kind, checked_left, checked_right, origin)
            .map_err(|error| Error::unsupported(error.message).at(origin))?,
        CombineKind::Join(kind) => {
            // A typed join is complete only when its predicate can be built
            // from checker facts. Let the query-lowering coordinator choose
            // the transitional adapter for other predicate forms.
            let predicate = plain_join_predicate(input, module, operation);
            let Some(predicate) = predicate else {
                return Ok(None);
            };
            CheckedQuery::join(kind, checked_left, checked_right, predicate, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?
        }
    };
    let output_row = Row::new(checked.row().columns().to_vec());
    let root = typed
        .combine(
            combine.node,
            kind,
            typed_left,
            typed_right,
            Some(output_row.clone()),
            origin,
            checked_join_predicate(&checked, origin),
        )
        .map_err(query_error)?;
    if checked.row().columns() != output_row.columns() {
        return Err(
            Error::unsupported("typed combine output row differs from its checked query")
                .at(origin),
        );
    }
    Ok(Some((typed.finish(root), checked)))
}

/// Lower a join predicate directly from checker-typed fields and literals.
/// Join fields retain their `left`/`right` side markers, so the checked join
/// constructor can validate them against the two child rows without consulting
/// a normalized relation.
fn plain_join_predicate(
    input: CompilerInput<'_>,
    module: usize,
    operation: &Expr,
) -> Option<crate::checked::CheckedExpr> {
    let ExprKind::App(_, args) = &operation.kind else {
        return None;
    };
    let predicate = args.get(1)?;
    lower_plain_expr(input, module, predicate).ok().flatten()
}

/// Lower a query whose syntax contains a combine and any number of ordinary
/// stages around it. The recursive walk follows the query structure in the
/// source: a pipe first lowers its left query, then applies either the combine
/// on its right or the right-hand stage. A query alias copies its already typed
/// graph, including both-input nodes.
fn lower_composed_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let mut typed = TypedQuery::new();
    if let Some((root, checked)) =
        lower_plain_composed_expr(context, input, module, def, body, &mut typed)?
    {
        return Ok(Some((typed.finish(root), checked)));
    }
    // Unsupported payloads decline here. The route coordinator owns the
    // transitional adapter for the whole definition.
    Ok(None)
}

/// Lower one of the prelude's pipeline operators (`&?`, `&=`, `&+`, `&*`,
/// `&.`, or `&-`). These are lambda helpers in source, so their stage records
/// belong to the prelude definition rather than the caller. The checker still
/// supplies the call-site result row and expression types; use those facts
/// directly instead of requiring a caller-local `RowStageView`.
fn lower_shorthand_stage(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    child: NodeId,
    mut checked: CheckedQuery,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    if args.len() != 2 || !is_stage_shorthand_head(head) {
        return Ok(None);
    }
    let ExprKind::Name(name) = &head.kind else {
        return Ok(None);
    };
    let origin = Origin::new(module, expr.span);
    let input_row = Row::new(checked.row().columns().to_vec());
    let expression_row = RowType::new(input_row.columns().to_vec());
    let output_row = input
        .type_check()
        .typed_node(module, expr.id)
        .and_then(|node| node.row())
        .map(Row::new)
        .ok_or_else(|| {
            Error::unsupported("shorthand stage has no checked output row").at(origin)
        })?;
    let site = NodeRef::new(module, def, expr.id);
    let (kind, predicate, keys, fields, count) = match name.as_str() {
        "_&?_" => {
            let Some(predicate) =
                lower_plain_expr_with_row(input, module, &args[1], Some(&expression_row))?
            else {
                return Ok(None);
            };
            (QueryNodeKind::Where, Some(predicate), None, None, None)
        }
        "_&=_" | "_&+_" | "_&*_" => {
            let ExprKind::Record(source_fields) = &args[1].kind else {
                return Ok(None);
            };
            let mut fields = Vec::with_capacity(source_fields.len());
            for (field, value) in source_fields {
                let Some(value) =
                    lower_plain_expr_with_row(input, module, value, Some(&expression_row))?
                else {
                    return Ok(None);
                };
                fields.push((field.clone(), value));
            }
            let kind = match name.as_str() {
                "_&=_" => QueryNodeKind::Select,
                "_&+_" => QueryNodeKind::Update,
                "_&*_" => QueryNodeKind::Agg,
                _ => unreachable!(),
            };
            (kind, None, None, Some(fields), None)
        }
        "_&._" => {
            let ExprKind::List(items) = &args[1].kind else {
                return Ok(None);
            };
            let mut keys = Vec::with_capacity(items.len());
            for item in items {
                let ExprKind::App(_, item_args) = &item.kind else {
                    return Ok(None);
                };
                let Some(value) = item_args.first() else {
                    return Ok(None);
                };
                let Some(direction) = sort_direction(input, module, item) else {
                    return Ok(None);
                };
                let Some(value) =
                    lower_plain_expr_with_row(input, module, value, Some(&expression_row))?
                else {
                    return Ok(None);
                };
                keys.push((value, direction));
            }
            (QueryNodeKind::Order, None, Some(keys), None, None)
        }
        "_&-_" => {
            let Some(Lit::Int(count)) = args.get(1).and_then(|arg| match &arg.kind {
                ExprKind::Lit(lit) => Some(lit),
                _ => None,
            }) else {
                return Ok(None);
            };
            (QueryNodeKind::Limit, None, None, None, Some(*count))
        }
        _ => return Ok(None),
    };
    let id = match kind {
        QueryNodeKind::Select | QueryNodeKind::Update | QueryNodeKind::Agg => typed
            .project(
                site,
                kind,
                child,
                Some(output_row.clone()),
                origin,
                fields.clone(),
            )
            .map_err(query_error)?,
        QueryNodeKind::Where | QueryNodeKind::Order | QueryNodeKind::Limit => typed
            .keep_row_with_payload(
                site,
                kind,
                child,
                origin,
                predicate.clone(),
                keys.clone(),
                None,
                None,
            )
            .map_err(query_error)?,
        _ => return Ok(None),
    };
    checked = match kind {
        QueryNodeKind::Where => {
            CheckedQuery::where_(checked, predicate.expect("predicate"), origin)
        }
        QueryNodeKind::Select => CheckedQuery::select(checked, fields.expect("fields"), origin),
        QueryNodeKind::Update => CheckedQuery::update(checked, fields.expect("fields"), origin),
        QueryNodeKind::Agg => CheckedQuery::agg(checked, fields.expect("fields"), origin),
        QueryNodeKind::Order => CheckedQuery::order(checked, keys.expect("keys"), origin),
        QueryNodeKind::Limit => CheckedQuery::limit(checked, count.expect("count"), origin),
        _ => unreachable!(),
    }
    .map_err(|error| Error::unsupported(error.message).at(origin))?;
    if checked.row().columns() != output_row.columns() {
        return Ok(None);
    }
    Ok(Some((id, checked)))
}

/// Apply one ordinary prelude stage expression to an already lowered child.
/// This is shared by direct pipelines and anonymous/named stage values so an
/// inline lambda does not require a normalized term to expose its body.
fn apply_plain_stage_expr(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    child: NodeId,
    mut checked: CheckedQuery,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    let Some(Resolved::Def((stage_module, stage_def))) =
        input.type_check().resolution(module, head.id)
    else {
        return Ok(None);
    };
    let ExprKind::Primitive(name) = &input.module(*stage_module).def(*stage_def).body.kind else {
        return Ok(None);
    };
    let Some(stage) = crate::primitive::prim_of(name).and_then(RowStage::of_prim) else {
        return Ok(None);
    };
    let origin = Origin::new(module, expr.span);
    let row = RowType::new(checked.row().columns().to_vec());
    let mut predicate = None;
    let mut keys = None;
    let mut fields = None;
    let mut label = None;
    let mut count = None;
    match stage {
        RowStage::Where => {
            let Some(value) = args.first() else {
                return Ok(None);
            };
            predicate = lower_plain_expr_with_row(input, module, value, Some(&row))?;
            if predicate.is_none() {
                return Ok(None);
            }
        }
        RowStage::Select | RowStage::Update | RowStage::Agg => {
            let Some(ExprKind::Record(source_fields)) = args.first().map(|arg| &arg.kind) else {
                return Ok(None);
            };
            let mut lowered = Vec::with_capacity(source_fields.len());
            for (name, value) in source_fields {
                let Some(value) = lower_plain_expr_with_row(input, module, value, Some(&row))?
                else {
                    return Ok(None);
                };
                lowered.push((name.clone(), value));
            }
            fields = Some(lowered);
        }
        RowStage::Order => {
            let Some(ExprKind::List(items)) = args.first().map(|arg| &arg.kind) else {
                return Ok(None);
            };
            let mut lowered = Vec::with_capacity(items.len());
            for item in items {
                let ExprKind::App(_, item_args) = &item.kind else {
                    return Ok(None);
                };
                let Some(value) = item_args.first() else {
                    return Ok(None);
                };
                let Some(direction) = sort_direction(input, module, item) else {
                    return Ok(None);
                };
                let Some(value) = lower_plain_expr_with_row(input, module, value, Some(&row))?
                else {
                    return Ok(None);
                };
                lowered.push((value, direction));
            }
            keys = Some(lowered);
        }
        RowStage::Limit | RowStage::Offset => {
            let Some(ExprKind::Lit(Lit::Int(value))) = args.first().map(|arg| &arg.kind) else {
                return Ok(None);
            };
            count = Some(*value);
        }
        RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
            let Some(value) = args.first().and_then(string_literal) else {
                return Ok(None);
            };
            label = Some(value);
        }
        RowStage::Distinct => {}
    }
    let site = input
        .type_check()
        .row_stages(module, def)
        .find(|record| record.site == expr.id)
        .map(|record| record.node)
        .unwrap_or_else(|| NodeRef::new(module, def, expr.id));
    let mut output_row = input
        .type_check()
        .typed_node(module, expr.id)
        .and_then(|node| node.row())
        .map(Row::new)
        .unwrap_or_else(|| Row::new(checked.row().columns().to_vec()));
    // A stage value is checked as a function, so its standalone typed node
    // does not always carry the row it will produce after receiving the
    // caller's concrete input. Projection fields do: their checked scalar
    // types are enough to reconstruct the output boundary before appending
    // the typed query node.
    if matches!(stage, RowStage::Select | RowStage::Agg | RowStage::Update) {
        if let Some(fields) = fields.as_ref() {
            let mut columns = match stage {
                RowStage::Update => checked.row().columns().to_vec(),
                _ => Vec::with_capacity(fields.len()),
            };
            for (name, value) in fields {
                if let Some((_, ty)) = columns.iter_mut().find(|(column, _)| column == name) {
                    *ty = value.ty().clone();
                } else {
                    columns.push((name.clone(), value.ty().clone()));
                }
            }
            output_row = Row::new(columns);
        }
    } else {
        output_row = Row::new(checked.row().columns().to_vec());
    }
    let kind = match stage {
        RowStage::Prefix => QueryNodeKind::Prefix,
        RowStage::Suffix => QueryNodeKind::Suffix,
        RowStage::Omit => QueryNodeKind::Omit,
        RowStage::MapValue => QueryNodeKind::MapValue,
        RowStage::Where => QueryNodeKind::Where,
        RowStage::Select => QueryNodeKind::Select,
        RowStage::Update => QueryNodeKind::Update,
        RowStage::Agg => QueryNodeKind::Agg,
        RowStage::Order => QueryNodeKind::Order,
        RowStage::Limit => QueryNodeKind::Limit,
        RowStage::Offset => QueryNodeKind::Offset,
        RowStage::Distinct => QueryNodeKind::Distinct,
    };
    let id = match stage {
        RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => typed
            .rewrite(
                site,
                kind,
                child,
                Some(Row::new(checked.row().columns().to_vec())),
                Some(output_row.clone()),
                origin,
            )
            .map_err(query_error)?,
        RowStage::Select | RowStage::Update | RowStage::Agg => typed
            .project(
                site,
                kind,
                child,
                Some(output_row.clone()),
                origin,
                fields.clone(),
            )
            .map_err(query_error)?,
        _ => typed
            .keep_row_with_payload(
                site,
                kind,
                child,
                origin,
                predicate.clone(),
                keys.clone(),
                None,
                None,
            )
            .map_err(query_error)?,
    };
    checked = match stage {
        RowStage::Prefix => CheckedQuery::prefix(checked, label.expect("label"), origin),
        RowStage::Suffix => CheckedQuery::suffix(checked, label.expect("label"), origin),
        RowStage::Omit => CheckedQuery::omit(checked, label.expect("label"), origin),
        RowStage::MapValue => CheckedQuery::map_value(checked, label.expect("label"), origin),
        RowStage::Where => CheckedQuery::where_(checked, predicate.expect("predicate"), origin),
        RowStage::Select => CheckedQuery::select(checked, fields.expect("fields"), origin),
        RowStage::Update => CheckedQuery::update(checked, fields.expect("fields"), origin),
        RowStage::Agg => CheckedQuery::agg(checked, fields.expect("fields"), origin),
        RowStage::Order => CheckedQuery::order(checked, keys.expect("keys"), origin),
        RowStage::Limit => CheckedQuery::limit(checked, count.expect("count"), origin),
        RowStage::Offset => CheckedQuery::offset(checked, count.expect("count"), origin),
        RowStage::Distinct => CheckedQuery::distinct(checked, origin),
    }
    .map_err(|error| Error::unsupported(error.message).at(origin))?;
    if checked.row().columns() != output_row.columns() {
        return Ok(None);
    }
    Ok(Some((id, checked)))
}

/// Source-build a composed query when all joins and stages have plain
/// expression payloads. The checked relation travels with each typed node, so
/// a join and its surrounding stages need no normalized query template.
fn lower_plain_composed_expr(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    if let Some((bound, checked)) = context.query_bindings.get(&(module, expr.id)) {
        let id = copy_typed_graph(bound, typed)?;
        return Ok(Some((id, checked.clone())));
    }
    if let Some((schema, table, columns)) = table_parts(input, module, expr) {
        let origin = table_origin(input, module, expr);
        let checked =
            CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let id = typed
            .table(NodeRef::new(module, def, expr.id), origin, columns)
            .map_err(query_error)?;
        return Ok(Some((id, checked)));
    }
    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if (target_module, target_def) == (module, def) {
            return Ok(None);
        }
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, checked)) =
            context.lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let id = copy_typed_graph(&target_typed, typed)?;
        return Ok(Some((id, checked)));
    }

    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    if args.len() == 2
        && matches!(&head.kind, ExprKind::Name(name) if name == "_&|_" || name == "_&~_")
    {
        let Some((left, checked_left)) =
            lower_plain_composed_expr(context, input, module, def, &args[0], typed)?
        else {
            return Ok(None);
        };
        let Some((right, checked_right)) =
            lower_plain_composed_expr(context, input, module, def, &args[1], typed)?
        else {
            return Ok(None);
        };
        let kind = if matches!(&head.kind, ExprKind::Name(name) if name == "_&|_") {
            crate::ir::SetKind::Union
        } else {
            crate::ir::SetKind::Except
        };
        let origin = Origin::new(module, expr.span);
        let checked = CheckedQuery::set(kind, checked_left, checked_right, origin)
            .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let output = Row::new(checked.row().columns().to_vec());
        let id = typed
            .combine(
                NodeRef::new(module, def, expr.id),
                QueryNodeKind::Set(kind),
                left,
                right,
                Some(output),
                origin,
                None,
            )
            .map_err(query_error)?;
        return Ok(Some((id, checked)));
    }
    if args.len() == 2 && is_stage_shorthand_head(head) {
        let Some((child, checked)) =
            lower_plain_composed_expr(context, input, module, def, &args[0], typed)?
        else {
            return Ok(None);
        };
        return lower_shorthand_stage(input, module, def, expr, child, checked, typed);
    }
    if args.len() != 2 || !is_pipe_head(head) {
        let mut helper_typed = TypedQuery::new();
        if let Some((root, checked)) =
            lower_helper_expr(context, input, module, def, expr, &mut helper_typed)?
        {
            let id = copy_typed_graph(&helper_typed.finish(root), typed)?;
            return Ok(Some((id, checked)));
        }
        return Ok(None);
    }
    let left_expr = &args[0];
    let rhs = &args[1];
    if let Some((stage_module, stage_expr)) = context.stage_bindings.get(&(module, rhs.id)) {
        let stage_expr = stage_expr.clone();
        let stage_module = *stage_module;
        let Some((child, checked)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        return apply_plain_stage_expr(
            input,
            stage_module,
            def,
            &stage_expr,
            child,
            checked,
            typed,
        );
    }
    if let Some(join_kind) = join_operator_kind(rhs) {
        let ExprKind::App(_, join_args) = &rhs.kind else {
            return Ok(None);
        };
        let (Some(right_expr), Some(predicate_expr)) = (join_args.first(), join_args.get(1)) else {
            return Ok(None);
        };
        let Some((left, checked_left)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        let Some((right, checked_right)) =
            lower_plain_composed_expr(context, input, module, def, right_expr, typed)?
        else {
            return Ok(None);
        };
        let Some(predicate) = lower_plain_expr(input, module, predicate_expr)? else {
            return Ok(None);
        };
        let origin = Origin::new(module, rhs.span);
        let checked = CheckedQuery::join(
            join_kind,
            checked_left,
            checked_right,
            predicate.clone(),
            origin,
        )
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let output = input
            .type_check()
            .typed_node(module, expr.id)
            .and_then(|node| node.row())
            .map(Row::new);
        let id = typed
            .combine(
                NodeRef::new(module, def, rhs.id),
                QueryNodeKind::Join(join_kind),
                left,
                right,
                output,
                origin,
                Some(predicate),
            )
            .map_err(query_error)?;
        return Ok(Some((id, checked)));
    }
    if let ExprKind::App(comp_head, comp_args) = &rhs.kind {
        if matches!(&comp_head.kind, ExprKind::Name(name) if name == "_>>>_")
            && comp_args.len() == 2
        {
            let Some((child, mut checked)) =
                lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
            else {
                return Ok(None);
            };
            let mut current = child;
            for stage_expr in comp_args.iter() {
                let Some((next, next_checked)) = apply_plain_stage_expr(
                    input, module, def, stage_expr, current, checked, typed,
                )?
                else {
                    return Ok(None);
                };
                current = next;
                checked = next_checked;
            }
            return Ok(Some((current, checked)));
        }
    }
    if let ExprKind::Lambda(parameter, body) = &rhs.kind {
        let Some((child, checked)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        if let Some(lowered) =
            lower_inline_stage_body(input, module, def, parameter, body, child, checked, typed)?
        {
            return Ok(Some(lowered));
        }
    }
    if let Some(stage_expr) = inline_stage_expr(rhs) {
        let Some((child, checked)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        return apply_plain_stage_expr(input, module, def, &stage_expr, child, checked, typed);
    }
    if let Some(((helper_module, helper_def), helper_args)) =
        helper_application(input.type_check(), module, rhs)
    {
        let Some((child, checked)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        if let Some(lowered) = apply_helper(
            context,
            input,
            module,
            def,
            expr.id,
            child,
            checked,
            helper_module,
            helper_def,
            0,
            &helper_args,
            rhs,
            typed,
        )? {
            return Ok(Some(lowered));
        }
    }
    if let Some((stage_module, stage_def)) = query_target(input.type_check(), module, rhs) {
        if matches!(
            input.type_check().scheme_view(stage_module, stage_def),
            Some(crate::check::SchemeView::Function)
        ) {
            let Some((child, checked)) =
                lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
            else {
                return Ok(None);
            };
            let stage_body = &input.module(stage_module).def(stage_def).body;
            let stage_body = lambda_tail(stage_body);
            if let ExprKind::App(comp_head, comp_args) = &stage_body.kind {
                if matches!(&comp_head.kind, ExprKind::Name(name) if name == "_>>>_")
                    && comp_args.len() == 2
                {
                    let mut current = child;
                    let mut current_checked = checked;
                    for stage_expr in comp_args.iter() {
                        let Some((next, next_checked)) = apply_plain_stage_expr(
                            input,
                            stage_module,
                            def,
                            stage_expr,
                            current,
                            current_checked,
                            typed,
                        )?
                        else {
                            return Ok(None);
                        };
                        current = next;
                        current_checked = next_checked;
                    }
                    return Ok(Some((current, current_checked)));
                }
            }
            return apply_plain_stage_expr(
                input,
                stage_module,
                def,
                stage_body,
                child,
                checked,
                typed,
            );
        }
    }
    if let Some((combine, operation)) = combine_record_in(input.type_check(), module, rhs) {
        let Some((left, checked_left)) =
            lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        let right_expr = match combine.kind {
            CombineKind::Join(_) => match &operation.kind {
                ExprKind::App(_, args) => args.first(),
                _ => None,
            },
            CombineKind::Set(_) => query_operand_args(input, module, operation).next(),
        };
        let Some(right_expr) = right_expr else {
            return Ok(None);
        };
        let Some((right, checked_right)) =
            lower_plain_composed_expr(context, input, module, def, right_expr, typed)?
        else {
            return Ok(None);
        };
        let origin = Origin::new(combine.node.definition_module, operation.span);
        let kind = combine_query_kind(combine.kind);
        let output = combine.output.clone().map(Row::new);
        let (typed_left, typed_right, checked) = match combine.kind {
            CombineKind::Join(join_kind) => {
                let ExprKind::App(_, join_args) = &operation.kind else {
                    return Ok(None);
                };
                let Some(predicate_expr) = join_args.get(1) else {
                    return Ok(None);
                };
                let Some(predicate) = lower_plain_expr(input, module, predicate_expr)? else {
                    return Ok(None);
                };
                let checked = CheckedQuery::join(
                    join_kind,
                    checked_left,
                    checked_right,
                    predicate.clone(),
                    origin,
                )
                .map_err(|error| Error::unsupported(error.message).at(origin))?;
                let id = typed
                    .combine(
                        combine.node,
                        kind,
                        left,
                        right,
                        output.or_else(|| Some(Row::new(checked.row().columns().to_vec()))),
                        origin,
                        Some(predicate),
                    )
                    .map_err(query_error)?;
                return Ok(Some((id, checked)));
            }
            CombineKind::Set(set_kind) => {
                // The source pipe supplies the first operand; set primitives
                // record their explicit argument first.
                (
                    right,
                    left,
                    CheckedQuery::set(set_kind, checked_right, checked_left, origin)
                        .map_err(|error| Error::unsupported(error.message).at(origin))?,
                )
            }
        };
        let id = typed
            .combine(
                combine.node,
                kind,
                typed_left,
                typed_right,
                output,
                origin,
                None,
            )
            .map_err(query_error)?;
        return Ok(Some((id, checked)));
    }

    // A query-valued helper can itself be one side of a combine. Reuse the
    // checker-driven helper pipeline here so its intermediate projection and
    // aggregate rows are instantiated before the combine consumes them.
    let mut helper_typed = TypedQuery::new();
    let helper_result = lower_helper_expr(context, input, module, def, expr, &mut helper_typed)?;
    if let Some((root, checked)) = helper_result {
        let id = copy_typed_graph(&helper_typed.finish(root), typed)?;
        return Ok(Some((id, checked)));
    }

    let Some((child, checked)) =
        lower_plain_composed_expr(context, input, module, def, left_expr, typed)?
    else {
        return Ok(None);
    };

    let stage = stage_record(input.type_check(), module, def, rhs.id)
        .filter(|stage| stage.node.definition_module == module && stage.node.definition == def);
    if stage.is_none() && stage_kind(input, module, rhs) == Some(RowStage::Distinct) {
        let origin = Origin::new(module, rhs.span);
        let id = typed
            .keep_row(
                NodeRef::new(module, def, rhs.id),
                QueryNodeKind::Distinct,
                child,
                origin,
            )
            .map_err(query_error)?;
        let checked = CheckedQuery::distinct(checked, origin)
            .map_err(|error| Error::unsupported(error.message).at(origin))?;
        return Ok(Some((id, checked)));
    }
    let Some(stage) = stage else {
        return Ok(None);
    };
    let ExprKind::App(_, stage_args) = &rhs.kind else {
        return Ok(None);
    };
    let origin = Origin::new(stage.node.definition_module, rhs.span);
    let input_row = checked.row().clone();
    if !rows_match(
        stage.kind,
        stage.input.as_deref(),
        Some(input_row.columns()),
    ) {
        return Ok(None);
    }
    let expression_row = RowType::new(input_row.columns().to_vec());
    let mut predicate = None;
    let mut keys = None;
    let mut fields = None;
    let mut label = stage.label.clone();
    let mut count = None;
    match stage.kind {
        RowStage::Where => {
            let Some(expr) = stage_args.first() else {
                return Ok(None);
            };
            predicate = lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?;
            if predicate.is_none() {
                return Ok(None);
            }
        }
        RowStage::Select | RowStage::Update | RowStage::Agg => {
            let Some(record) = stage_args.first() else {
                return Ok(None);
            };
            let ExprKind::Record(source_fields) = &record.kind else {
                return Ok(None);
            };
            let mut lowered = Vec::with_capacity(source_fields.len());
            for (name, expr) in source_fields {
                let Some(expr) =
                    lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?
                else {
                    return Ok(None);
                };
                lowered.push((name.clone(), expr));
            }
            fields = Some(lowered);
        }
        RowStage::Order => {
            let Some(list) = stage_args.first() else {
                return Ok(None);
            };
            let ExprKind::List(items) = &list.kind else {
                return Ok(None);
            };
            let mut lowered = Vec::with_capacity(items.len());
            for item in items {
                let ExprKind::App(_, item_args) = &item.kind else {
                    return Ok(None);
                };
                let Some(expr) = item_args.first() else {
                    return Ok(None);
                };
                let Some(ascending) = sort_direction(input, module, item) else {
                    return Ok(None);
                };
                let Some(expr) =
                    lower_plain_expr_with_row(input, module, expr, Some(&expression_row))?
                else {
                    return Ok(None);
                };
                lowered.push((expr, ascending));
            }
            keys = Some(lowered);
        }
        RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
            if label.is_none() {
                return Ok(None);
            }
        }
        RowStage::Limit | RowStage::Offset => {
            count = stage_count(&rhs);
            if count.is_none() {
                let argument = stage_args.first().map(|arg| arg.span).unwrap_or(rhs.span);
                return Err(Error::new("limit expects an int").at(Origin::new(module, argument)));
            }
        }
        RowStage::Distinct => {}
    }
    let kind = match stage.kind {
        RowStage::Prefix => QueryNodeKind::Prefix,
        RowStage::Suffix => QueryNodeKind::Suffix,
        RowStage::Omit => QueryNodeKind::Omit,
        RowStage::MapValue => QueryNodeKind::MapValue,
        RowStage::Where => QueryNodeKind::Where,
        RowStage::Select => QueryNodeKind::Select,
        RowStage::Update => QueryNodeKind::Update,
        RowStage::Agg => QueryNodeKind::Agg,
        RowStage::Order => QueryNodeKind::Order,
        RowStage::Limit => QueryNodeKind::Limit,
        RowStage::Offset => QueryNodeKind::Offset,
        RowStage::Distinct => QueryNodeKind::Distinct,
    };
    let id = match stage.kind {
        RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => typed
            .rewrite(
                stage.node,
                kind,
                child,
                stage.input.clone().map(Row::new),
                stage.output.clone().map(Row::new),
                origin,
            )
            .map_err(query_error)?,
        RowStage::Select | RowStage::Update | RowStage::Agg => typed
            .project(
                stage.node,
                kind,
                child,
                stage.output.clone().map(Row::new),
                origin,
                fields.clone(),
            )
            .map_err(query_error)?,
        _ => typed
            .keep_row_with_payload(
                stage.node,
                kind,
                child,
                origin,
                predicate.clone(),
                keys.clone(),
                None,
                None,
            )
            .map_err(query_error)?,
    };
    let checked = match stage.kind {
        RowStage::Prefix => CheckedQuery::prefix(checked, label.take().expect("label"), origin),
        RowStage::Suffix => CheckedQuery::suffix(checked, label.take().expect("label"), origin),
        RowStage::Omit => CheckedQuery::omit(checked, label.take().expect("label"), origin),
        RowStage::MapValue => {
            CheckedQuery::map_value(checked, label.take().expect("label"), origin)
        }
        RowStage::Where => CheckedQuery::where_(checked, predicate.expect("predicate"), origin),
        RowStage::Select => CheckedQuery::select(checked, fields.expect("fields"), origin),
        RowStage::Update => CheckedQuery::update(checked, fields.expect("fields"), origin),
        RowStage::Agg => CheckedQuery::agg(checked, fields.expect("fields"), origin),
        RowStage::Order => CheckedQuery::order(checked, keys.expect("keys"), origin),
        RowStage::Limit => CheckedQuery::limit(checked, count.expect("count"), origin),
        RowStage::Offset => CheckedQuery::offset(checked, count.expect("count"), origin),
        RowStage::Distinct => CheckedQuery::distinct(checked, origin),
    }
    .map_err(|error| Error::unsupported(error.message).at(origin))?;
    let node = typed.node(id);
    if !rows_match(
        stage.kind,
        stage.input.as_deref(),
        node.input_row().map(Row::columns),
    ) || !rows_match(
        stage.kind,
        stage.output.as_deref(),
        node.output_row().map(Row::columns),
    ) {
        return Ok(None);
    }
    Ok(Some((id, checked)))
}

fn lower_composed_expr(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<NodeId>, Error> {
    if let Some((schema, table, columns)) = table_parts(input, module, expr) {
        let origin = Origin::new(module, expr.span);
        let id = typed
            .table(NodeRef::new(module, def, expr.id), origin, columns)
            .map_err(query_error)?;
        let _ = (schema, table); // TypedQuery records the checker-owned columns.
        return Ok(Some(id));
    }

    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if (target_module, target_def) == (module, def) {
            return Ok(None);
        }
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, _)) =
            context.lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        return copy_typed_graph(&target_typed, typed).map(Some);
    }

    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };

    if args.len() == 2 && is_pipe_head(head) {
        // A helper stage is a query-valued function, not a primitive row
        // stage, so it has no `RowStageView` at the use site. Let the helper
        // instantiation path build its own typed nodes before this expression
        // is used as one side of a combine.
        if let Some(id) = lower_composed_helper(context, input, module, def, expr, typed)? {
            return Ok(Some(id));
        }
        let rhs = &args[1];
        let Some(left) = lower_composed_expr(context, input, module, def, &args[0], typed)? else {
            return Ok(None);
        };
        if let Some((combine, operation)) = combine_record_in(input.type_check(), module, rhs) {
            let Some(right_expr) = query_operand_args(input, module, operation).next() else {
                return Ok(None);
            };
            let Some(right) = lower_composed_expr(context, input, module, def, right_expr, typed)?
            else {
                return Ok(None);
            };
            let kind = combine_query_kind(combine.kind);
            let output = combine.output.clone().map(Row::new);
            let origin = Origin::new(combine.node.definition_module, operation.span);
            let join_predicate = match combine.kind {
                CombineKind::Join(_) => {
                    let Some(predicate) = plain_join_predicate(input, module, operation) else {
                        return Ok(None);
                    };
                    Some(predicate)
                }
                CombineKind::Set(_) => None,
            };
            let id = typed
                .combine(
                    combine.node,
                    kind,
                    left,
                    right,
                    output,
                    origin,
                    join_predicate,
                )
                .map_err(query_error)?;
            return Ok(Some(id));
        }
        if let Some(stage) = stage_record(input.type_check(), module, def, rhs.id)
            .filter(|stage| stage.node.definition_module == module && stage.node.definition == def)
        {
            return lower_composed_stage(input, module, def, rhs, left, stage, typed);
        }
        return Ok(None);
    }

    // A direct call such as `union users vips` has two query arguments. The
    // same combine record supplies its ordered operands and checked predicate.
    if let Some((combine, operation)) = combine_record_in(input.type_check(), module, expr) {
        let operands: Vec<_> = query_operand_args(input, module, operation).collect();
        if operands.len() != 2 {
            return Ok(None);
        }
        let Some(left) = lower_composed_expr(context, input, module, def, operands[0], typed)?
        else {
            return Ok(None);
        };
        let Some(right) = lower_composed_expr(context, input, module, def, operands[1], typed)?
        else {
            return Ok(None);
        };
        let kind = combine_query_kind(combine.kind);
        let output = combine.output.clone().map(Row::new);
        let origin = Origin::new(combine.node.definition_module, operation.span);
        let join_predicate = match combine.kind {
            CombineKind::Join(_) => {
                let Some(predicate) = plain_join_predicate(input, module, operation) else {
                    return Ok(None);
                };
                Some(predicate)
            }
            CombineKind::Set(_) => None,
        };
        let id = typed
            .combine(
                combine.node,
                kind,
                left,
                right,
                output,
                origin,
                join_predicate,
            )
            .map_err(query_error)?;
        return Ok(Some(id));
    }

    if let Some(id) = lower_composed_helper(context, input, module, def, expr, typed)? {
        return Ok(Some(id));
    }
    Ok(None)
}

fn lower_composed_helper(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    target: &mut TypedQuery,
) -> Result<Option<NodeId>, Error> {
    // Helper instantiation already has a checker-driven implementation that
    // threads the concrete row and payload through every stage. Reuse it for
    // combine operands so a helper with multiple projections or aggregates is
    // not downgraded merely because it appears below a join or set operation.
    let mut source = TypedQuery::new();
    let Some((root, _)) = lower_helper_expr(context, input, module, def, expr, &mut source)? else {
        return Ok(None);
    };
    copy_typed_graph(&source.finish(root), target).map(Some)
}

fn typed_keep_stage(
    typed: &mut TypedQuery,
    child: NodeId,
    stage: RowStage,
    site: NodeRef,
    origin: Origin,
) -> Result<NodeId, Error> {
    let kind = match stage {
        RowStage::Where => QueryNodeKind::Where,
        RowStage::Order => QueryNodeKind::Order,
        RowStage::Limit => QueryNodeKind::Limit,
        RowStage::Offset => QueryNodeKind::Offset,
        RowStage::Distinct => QueryNodeKind::Distinct,
        _ => return Err(Error::unsupported("helper stage is not row-preserving").at(origin)),
    };
    typed
        .keep_row(site, kind, child, origin)
        .map_err(query_error)
}

fn lower_composed_stage(
    _input: CompilerInput<'_>,
    _module: usize,
    _def: usize,
    rhs: &Expr,
    child: NodeId,
    stage: crate::check::RowStageView,
    typed: &mut TypedQuery,
) -> Result<Option<NodeId>, Error> {
    let origin = Origin::new(stage.node.definition_module, rhs.span);
    let input_row = stage.input.clone().map(Row::new);
    let output_row = stage.output.clone().map(Row::new);
    let id = match stage.kind {
        RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => typed
            .rewrite(
                stage.node,
                match stage.kind {
                    RowStage::Prefix => QueryNodeKind::Prefix,
                    RowStage::Suffix => QueryNodeKind::Suffix,
                    RowStage::Omit => QueryNodeKind::Omit,
                    RowStage::MapValue => QueryNodeKind::MapValue,
                    _ => unreachable!(),
                },
                child,
                input_row,
                output_row,
                origin,
            )
            .map_err(query_error)?,
        RowStage::Select | RowStage::Update | RowStage::Agg => typed
            .project(
                stage.node,
                match stage.kind {
                    RowStage::Select => QueryNodeKind::Select,
                    RowStage::Update => QueryNodeKind::Update,
                    RowStage::Agg => QueryNodeKind::Agg,
                    _ => unreachable!(),
                },
                child,
                output_row,
                origin,
                None,
            )
            .map_err(query_error)?,
        RowStage::Where
        | RowStage::Order
        | RowStage::Limit
        | RowStage::Offset
        | RowStage::Distinct => typed
            .keep_row(
                stage.node,
                match stage.kind {
                    RowStage::Where => QueryNodeKind::Where,
                    RowStage::Order => QueryNodeKind::Order,
                    RowStage::Limit => QueryNodeKind::Limit,
                    RowStage::Offset => QueryNodeKind::Offset,
                    RowStage::Distinct => QueryNodeKind::Distinct,
                    _ => unreachable!(),
                },
                child,
                origin,
            )
            .map_err(query_error)?,
    };
    let node = typed.node(id);
    if !rows_match(
        stage.kind,
        stage.input.as_deref(),
        node.input_row().map(Row::columns),
    ) || !rows_match(
        stage.kind,
        stage.output.as_deref(),
        node.output_row().map(Row::columns),
    ) {
        return Ok(None);
    }
    Ok(Some(id))
}

fn combine_query_kind(kind: CombineKind) -> QueryNodeKind {
    match kind {
        CombineKind::Join(kind) => QueryNodeKind::Join(kind),
        CombineKind::Set(kind) => QueryNodeKind::Set(kind),
    }
}

fn checked_join_predicate(
    checked: &CheckedQuery,
    origin: Origin,
) -> Option<crate::checked::CheckedExpr> {
    let joins: Vec<_> = checked
        .node_chain_join_predicates()
        .into_iter()
        .filter(|(kind, _, _)| *kind == crate::checked::QueryKind::Join)
        .collect();
    joins
        .iter()
        .find(|(_, node_origin, _)| *node_origin == origin)
        .or_else(|| (joins.len() == 1).then(|| &joins[0]))
        .and_then(|(_, _, predicate)| predicate.clone())
}

fn contains_combine_record(tc: &crate::check::TypeCheck, module: usize, expr: &Expr) -> bool {
    combine_record_in(tc, module, expr).is_some()
}

fn contains_combine_tree(input: CompilerInput<'_>, module: usize, expr: &Expr) -> bool {
    fn visit(
        input: CompilerInput<'_>,
        module: usize,
        expr: &Expr,
        seen: &mut HashSet<(usize, usize)>,
    ) -> bool {
        let tc = input.type_check();
        if contains_combine_record(tc, module, expr) {
            return true;
        }
        if let Some((target_module, target_def)) = query_target(tc, module, expr) {
            if seen.insert((target_module, target_def)) {
                let target_body = &input.module(target_module).def(target_def).body;
                if visit(input, target_module, target_body, seen) {
                    return true;
                }
            }
        }
        crate::check::child_exprs(expr)
            .into_iter()
            .any(|child| visit(input, module, child, seen))
    }

    visit(input, module, expr, &mut HashSet::new())
}

fn contains_query_source(input: CompilerInput<'_>, module: usize, expr: &Expr) -> bool {
    if table_parts(input, module, expr).is_some() {
        return true;
    }
    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if matches!(
            input.type_check().scheme_view(target_module, target_def),
            Some(crate::check::SchemeView::Query { .. })
        ) {
            return true;
        }
    }
    crate::check::child_exprs(expr)
        .into_iter()
        .any(|child| contains_query_source(input, module, child))
}

fn combine_record_in<'a>(
    tc: &crate::check::TypeCheck,
    module: usize,
    expr: &'a Expr,
) -> Option<(crate::check::CombineView, &'a Expr)> {
    if let Some(combine) = tc
        .typed_node(module, expr.id)
        .and_then(|node| node.combine.clone())
    {
        return Some((combine, expr));
    }
    crate::check::child_exprs(expr)
        .into_iter()
        .find_map(|child| combine_record_in(tc, module, child))
}

fn query_operand_args<'a>(
    input: CompilerInput<'a>,
    module: usize,
    operation: &'a Expr,
) -> Box<dyn Iterator<Item = &'a Expr> + 'a> {
    let tc = input.type_check();
    let ExprKind::App(_, args) = &operation.kind else {
        return Box::new(std::iter::empty());
    };
    Box::new(args.iter().filter(move |arg| {
        table_parts(input, module, arg).is_some()
            || tc
                .typed_node(module, arg.id)
                .is_some_and(|node| node.row().is_some())
            || query_target(tc, module, arg).is_some()
    }))
}

fn stage_record(
    tc: &crate::check::TypeCheck,
    module: usize,
    def: usize,
    site: u32,
) -> Option<crate::check::RowStageView> {
    tc.row_stages(module, def)
        .find(|stage| stage.site == site)
        .cloned()
}

fn lower_combine_input(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    if let Some((schema, table, columns)) = table_parts(input, module, expr) {
        let origin = table_origin(input, module, expr);
        let checked =
            CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let id = typed
            .table(NodeRef::new(module, def, expr.id), origin, columns)
            .map_err(query_error)?;
        return Ok(Some((id, checked)));
    }
    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, checked)) =
            context.lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let id = copy_typed_graph(&target_typed, typed)?;
        return Ok(Some((id, checked)));
    }
    Ok(None)
}

fn table_origin(input: CompilerInput<'_>, module: usize, expr: &Expr) -> Origin {
    fn visit(
        input: CompilerInput<'_>,
        module: usize,
        expr: &Expr,
        seen: &mut HashSet<(usize, usize)>,
    ) -> Origin {
        match &expr.kind {
            ExprKind::App(_, _) => Origin::new(module, expr.span),
            ExprKind::Name(_) | ExprKind::Proj(_, _) => {
                if let Some(Resolved::Def((target_module, target_def))) =
                    input.type_check().resolution(module, expr.id)
                {
                    if seen.insert((*target_module, *target_def)) {
                        let target = &input.module(*target_module).def(*target_def).body;
                        return visit(input, *target_module, target, seen);
                    }
                }
                Origin::new(module, expr.span)
            }
            _ => Origin::new(module, expr.span),
        }
    }

    visit(input, module, expr, &mut HashSet::new())
}

fn copy_typed_graph(source: &TypedQuery, target: &mut TypedQuery) -> Result<NodeId, Error> {
    let mut copied = HashMap::new();
    let source_ids: Vec<_> = source.nodes().map(|node| node.id()).collect();
    for old in source_ids {
        let node = source.node(old);
        let new = match node.kind() {
            QueryNodeKind::Table => target
                .table(
                    node.site(),
                    node.origin(),
                    node.output_row()
                        .ok_or_else(|| Error::unsupported("typed table has no output row"))?
                        .columns()
                        .to_vec(),
                )
                .map_err(query_error)?,
            QueryNodeKind::Join(_) | QueryNodeKind::Set(_) => {
                let inputs = node.inputs();
                let left = *copied
                    .get(&inputs[0])
                    .ok_or_else(|| Error::unsupported("typed combine left child was not copied"))?;
                let right = *copied.get(&inputs[1]).ok_or_else(|| {
                    Error::unsupported("typed combine right child was not copied")
                })?;
                target
                    .combine(
                        node.site(),
                        node.kind(),
                        left,
                        right,
                        node.output_row().cloned(),
                        node.origin(),
                        node.join_predicate().cloned(),
                    )
                    .map_err(query_error)?
            }
            QueryNodeKind::Prefix
            | QueryNodeKind::Suffix
            | QueryNodeKind::Omit
            | QueryNodeKind::MapValue => {
                let child = *copied
                    .get(
                        &node
                            .input()
                            .ok_or_else(|| Error::unsupported("typed stage has no child"))?,
                    )
                    .ok_or_else(|| Error::unsupported("typed stage child was not copied"))?;
                target
                    .rewrite(
                        node.site(),
                        node.kind(),
                        child,
                        node.input_row().cloned(),
                        node.output_row().cloned(),
                        node.origin(),
                    )
                    .map_err(query_error)?
            }
            QueryNodeKind::Select | QueryNodeKind::Update | QueryNodeKind::Agg => {
                let child = *copied
                    .get(
                        &node
                            .input()
                            .ok_or_else(|| Error::unsupported("typed projection has no child"))?,
                    )
                    .ok_or_else(|| Error::unsupported("typed projection child was not copied"))?;
                target
                    .project(
                        node.site(),
                        node.kind(),
                        child,
                        node.output_row().cloned(),
                        node.origin(),
                        node.projection_fields().map(<[_]>::to_vec),
                    )
                    .map_err(query_error)?
            }
            QueryNodeKind::Where
            | QueryNodeKind::Order
            | QueryNodeKind::Limit
            | QueryNodeKind::Offset
            | QueryNodeKind::Distinct => {
                let child = *copied
                    .get(
                        &node
                            .input()
                            .ok_or_else(|| Error::unsupported("typed stage has no child"))?,
                    )
                    .ok_or_else(|| Error::unsupported("typed stage child was not copied"))?;
                target
                    .keep_row_with_payload(
                        node.site(),
                        node.kind(),
                        child,
                        node.origin(),
                        node.predicate().cloned(),
                        node.order_keys().map(<[_]>::to_vec),
                        None,
                        None,
                    )
                    .map_err(query_error)?
            }
        };
        copied.insert(old, new);
    }
    copied
        .get(&source.root())
        .copied()
        .ok_or_else(|| Error::unsupported("typed query has no root"))
}

fn find_combine_context<'a>(expr: &'a Expr, site: u32) -> Option<(&'a Expr, &'a Expr)> {
    let ExprKind::App(head, args) = &expr.kind else {
        return None;
    };
    if args.len() == 2 && is_pipe_head(head) && contains_expr_id(&args[1], site) {
        return find_expr_id(&args[1], site).map(|operation| (&args[0], operation));
    }
    for child in crate::check::child_exprs(expr) {
        if let Some(found) = find_combine_context(child, site) {
            return Some(found);
        }
    }
    None
}

fn contains_expr_id(expr: &Expr, id: u32) -> bool {
    expr.id == id
        || crate::check::child_exprs(expr)
            .into_iter()
            .any(|child| contains_expr_id(child, id))
}

fn find_expr_id(expr: &Expr, id: u32) -> Option<&Expr> {
    if expr.id == id {
        return Some(expr);
    }
    crate::check::child_exprs(expr)
        .into_iter()
        .find_map(|child| find_expr_id(child, id))
}

fn expr_depth_to_id(expr: &Expr, id: u32, depth: usize) -> Option<usize> {
    if expr.id == id {
        return Some(depth);
    }
    crate::check::child_exprs(expr)
        .into_iter()
        .find_map(|child| expr_depth_to_id(child, id, depth + 1))
}

fn is_direct_pipeline(input: CompilerInput<'_>, module: usize, expr: &Expr) -> bool {
    if table_parts(input, module, expr).is_some() {
        return true;
    }
    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if matches!(
            input.type_check().scheme_view(target_module, target_def),
            Some(crate::check::SchemeView::Query { .. })
        ) {
            return true;
        }
    }
    let ExprKind::App(head, args) = &expr.kind else {
        return false;
    };
    if args.len() != 2 || !is_pipe_head(head) {
        return false;
    }
    if !is_direct_pipeline(input, module, &args[0]) {
        return false;
    }
    let (stage_head, _) = match &args[1].kind {
        ExprKind::App(stage_head, stage_args) => (&**stage_head, &stage_args[..]),
        ExprKind::Name(_) | ExprKind::Proj(_, _) => (&args[1], &[][..]),
        _ => return false,
    };
    let Some(Resolved::Def((stage_module, stage_def))) =
        input.type_check().resolution(module, stage_head.id)
    else {
        return false;
    };
    let ExprKind::Primitive(name) = &input.module(*stage_module).def(*stage_def).body.kind else {
        return false;
    };
    crate::primitive::prim_of(name)
        .and_then(RowStage::of_prim)
        .is_some()
}

fn rows_match(
    kind: RowStage,
    expected: Option<&[(String, crate::core::ScalarType)]>,
    actual: Option<&[(String, crate::core::ScalarType)]>,
) -> bool {
    match (expected, actual) {
        // An open checker row carries no boundary to compare. A typed helper
        // may still have a concrete row after instantiation, which is valid.
        (None, _) => true,
        (Some(expected), Some(actual))
            if matches!(
                kind,
                RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
            ) =>
        {
            expected == actual
        }
        (Some(expected), Some(actual)) => {
            let mut expected = expected.to_vec();
            let mut actual = actual.to_vec();
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            expected == actual
        }
        _ => false,
    }
}

/// Lower the expression-free row-preserving slice. These stages are not
/// checker row records yet: their boundaries are derived by the typed child,
/// while the checker still owns the full expression-bearing stage bridge.
fn lower_keep_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let mut typed = TypedQuery::new();
    let Some((root, checked)) = lower_keep_expr(context, input, module, def, body, &mut typed)?
    else {
        return Ok(None);
    };
    if typed.nodes().any(|node| {
        !matches!(
            node.kind(),
            QueryNodeKind::Table
                | QueryNodeKind::Limit
                | QueryNodeKind::Offset
                | QueryNodeKind::Distinct
                | QueryNodeKind::Prefix
                | QueryNodeKind::Suffix
                | QueryNodeKind::Omit
                | QueryNodeKind::MapValue
        )
    }) {
        return Ok(None);
    }
    Ok(Some((typed.finish(root), checked)))
}

fn lower_keep_expr(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    // Keep an already typed query alias as the child when expression-free
    // stages are appended to it. Resolving only to the backing table would
    // discard the alias's relational chain.
    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if (target_module, target_def) != (module, def)
            && matches!(
                input.type_check().scheme_view(target_module, target_def),
                Some(crate::check::SchemeView::Query { .. })
            )
        {
            let target_body = &input.module(target_module).def(target_def).body;
            let Some((target_typed, checked)) =
                context.lower(input, target_module, target_def, target_body)?
            else {
                return Ok(None);
            };
            let root = copy_typed_chain(&target_typed, typed)?;
            return Ok(Some((root, checked)));
        }
    }
    if let Some((schema, table, columns)) = table_parts(input, module, expr) {
        let origin = Origin::new(module, expr.span);
        let checked =
            CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let child = typed
            .table(NodeRef::new(module, def, expr.id), origin, columns)
            .map_err(query_error)?;
        return Ok(Some((child, checked)));
    }
    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    if args.len() != 2 || !is_pipe_head(head) {
        return Ok(None);
    }
    let Some((child, mut checked)) = lower_keep_expr(context, input, module, def, &args[0], typed)?
    else {
        return Ok(None);
    };
    let (stage_head, stage_args) = match &args[1].kind {
        ExprKind::App(stage_head, stage_args) => (&**stage_head, &stage_args[..]),
        ExprKind::Name(_) | ExprKind::Proj(_, _) => (&args[1], &[][..]),
        _ => return Ok(None),
    };
    let Some(Resolved::Def((stage_module, stage_def))) =
        input.type_check().resolution(module, stage_head.id)
    else {
        return Ok(None);
    };
    let ExprKind::Primitive(name) = &input.module(*stage_module).def(*stage_def).body.kind else {
        return Ok(None);
    };
    let kind = match crate::primitive::prim_of(name) {
        Some(crate::primitive::Prim::Limit) if stage_args.len() == 1 => QueryNodeKind::Limit,
        Some(crate::primitive::Prim::Offset) if stage_args.len() == 1 => QueryNodeKind::Offset,
        Some(crate::primitive::Prim::Distinct) if stage_args.is_empty() => QueryNodeKind::Distinct,
        Some(crate::primitive::Prim::Prefix) if stage_args.len() == 1 => QueryNodeKind::Prefix,
        Some(crate::primitive::Prim::Suffix) if stage_args.len() == 1 => QueryNodeKind::Suffix,
        Some(crate::primitive::Prim::Omit) if stage_args.len() == 1 => QueryNodeKind::Omit,
        Some(crate::primitive::Prim::MapValue) if stage_args.len() == 1 => QueryNodeKind::MapValue,
        _ => return Ok(None),
    };
    let origin = Origin::new(module, args[1].span);
    let (id, rewrite_label) = match kind {
        QueryNodeKind::Prefix
        | QueryNodeKind::Suffix
        | QueryNodeKind::Omit
        | QueryNodeKind::MapValue => {
            let Some(record) = input
                .type_check()
                .row_stages(module, def)
                .find(|record| record.site == args[1].id)
                .cloned()
            else {
                return Ok(None);
            };
            let (Some(input_row), Some(output_row), Some(label)) =
                (record.input, record.output, record.label)
            else {
                return Ok(None);
            };
            let id = typed
                .rewrite(
                    record.node,
                    kind,
                    child,
                    Some(Row::new(input_row)),
                    Some(Row::new(output_row)),
                    origin,
                )
                .map_err(query_error)?;
            (id, Some(label))
        }
        _ => {
            let site = NodeRef::new(module, def, args[1].id);
            (
                typed
                    .keep_row(site, kind, child, origin)
                    .map_err(query_error)?,
                None,
            )
        }
    };
    checked = match kind {
        QueryNodeKind::Limit => {
            let Some(ExprKind::Lit(Lit::Int(count))) = stage_args.first().map(|arg| &arg.kind)
            else {
                return Ok(None);
            };
            CheckedQuery::limit(checked, *count, origin)
        }
        QueryNodeKind::Offset => {
            let Some(ExprKind::Lit(Lit::Int(count))) = stage_args.first().map(|arg| &arg.kind)
            else {
                return Ok(None);
            };
            CheckedQuery::offset(checked, *count, origin)
        }
        QueryNodeKind::Distinct => CheckedQuery::distinct(checked, origin),
        QueryNodeKind::Prefix => {
            CheckedQuery::prefix(checked, rewrite_label.expect("rewrite label"), origin)
        }
        QueryNodeKind::Suffix => {
            CheckedQuery::suffix(checked, rewrite_label.expect("rewrite label"), origin)
        }
        QueryNodeKind::Omit => {
            CheckedQuery::omit(checked, rewrite_label.expect("rewrite label"), origin)
        }
        QueryNodeKind::MapValue => {
            CheckedQuery::map_value(checked, rewrite_label.expect("rewrite label"), origin)
        }
        _ => unreachable!(),
    }
    .map_err(|error| Error::unsupported(error.message).at(origin))?;
    Ok(Some((id, checked)))
}

fn lower_helper_pipeline(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let mut typed = TypedQuery::new();
    let Some((root, checked)) = lower_helper_expr(context, input, module, def, body, &mut typed)?
    else {
        return Ok(None);
    };
    Ok(Some((typed.finish(root), checked)))
}

fn lower_helper_expr(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let tc = input.type_check();

    // Resolve query aliases before treating a name as a table leaf. This keeps
    // the alias's typed stage chain (rather than reducing it to the table it
    // eventually names) when the alias is composed with another helper.
    if let Some((target_module, target_def)) = query_target(tc, module, body) {
        if (target_module, target_def) == (module, def) {
            return Ok(None);
        }
        if !matches!(
            tc.scheme_view(target_module, target_def),
            Some(crate::check::SchemeView::Query { .. })
        ) {
            return Ok(None);
        }
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, checked)) =
            context.lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let root = copy_typed_chain(&target_typed, typed)?;
        return Ok(Some((root, checked)));
    }

    if let Some((schema, table, columns)) = table_parts(input, module, body) {
        let origin = Origin::new(module, body.span);
        let checked =
            CheckedQuery::table(schema, table, Some(RowType::new(columns.clone())), origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let child = typed
            .table(NodeRef::new(module, def, body.id), origin, columns)
            .map_err(query_error)?;
        return Ok(Some((child, checked)));
    }

    if let ExprKind::App(head, args) = &body.kind {
        if args.len() >= 2 && is_stage_shorthand_head(head) {
            let Some((child, checked)) =
                lower_helper_expr(context, input, module, def, &args[0], typed)?
            else {
                return Ok(None);
            };
            return lower_shorthand_stage(input, module, def, body, child, checked, typed);
        }
        // A query helper may be called in ordinary prefix form (`h users`)
        // as well as through the pipeline spelling (`users & h`).  The
        // checker has already recorded the helper's stage instances; locate
        // the query argument and instantiate those stages against its typed
        // child here.
        if !is_pipe_head(head) {
            if let Some(((helper_module, helper_def), helper_args)) =
                helper_application(tc, module, body)
            {
                if let Some((query_index, query_arg)) =
                    helper_args.iter().enumerate().find(|(_, arg)| {
                        tc.use_scheme(module, arg.id).is_some_and(|scheme| {
                            matches!(scheme, crate::check::SchemeView::Query { .. })
                        }) || query_target(tc, module, arg).is_some()
                            || table_parts(input, module, arg).is_some()
                    })
                {
                    let Some((child, checked)) =
                        lower_helper_expr(context, input, module, def, query_arg, typed)?
                    else {
                        return Ok(None);
                    };
                    let mut stage_args = helper_args;
                    stage_args.remove(query_index);
                    return apply_helper(
                        context,
                        input,
                        module,
                        def,
                        body.id,
                        child,
                        checked,
                        helper_module,
                        helper_def,
                        query_index,
                        &stage_args,
                        body,
                        typed,
                    );
                }
                // A higher-order helper can own its query source while taking
                // a stage function as an argument (`h = p => users & ... & p`).
                // Bind each stage parameter occurrence to the checked use-site
                // expression, then lower the helper's query body in place.
                let helper_syntax = &input.module(helper_module).def(helper_def).body;
                let params = lambda_params(helper_syntax);
                let helper_tail = lambda_tail(helper_syntax);
                if contains_query_source(input, helper_module, helper_tail)
                    && helper_args.len() <= params.len()
                {
                    // Substitute scalar values, while stage-function arguments
                    // remain bound at their source nodes so stage lowering reads
                    // checker records from the caller's module.
                    let mut substitutions = HashMap::new();
                    let mut bound_ids = Vec::new();
                    let mut helper_nodes = HashMap::new();
                    index_exprs(helper_tail, &mut helper_nodes);
                    for (parameter, argument) in params.iter().zip(&helper_args) {
                        let function_argument =
                            tc.use_scheme(module, argument.id).is_some_and(|scheme| {
                                matches!(scheme, crate::check::SchemeView::Function)
                            });
                        if function_argument {
                            for expr in helper_nodes.values() {
                                if matches!(&expr.kind, ExprKind::Name(name) if name == parameter) {
                                    context.stage_bindings.insert(
                                        (helper_module, expr.id),
                                        (module, argument.clone()),
                                    );
                                    bound_ids.push(expr.id);
                                }
                            }
                        } else {
                            substitutions.insert(parameter.as_str(), argument.clone());
                        }
                    }
                    let substituted = substitute_expr(helper_tail, &substitutions);
                    let lowered = lower_plain_composed_expr(
                        context,
                        input,
                        helper_module,
                        helper_def,
                        &substituted,
                        typed,
                    )?;
                    for id in bound_ids {
                        context.stage_bindings.remove(&(helper_module, id));
                    }
                    if lowered.is_some() {
                        return Ok(lowered);
                    }
                }
            }
        }
        if args.len() == 2 && is_pipe_head(head) {
            let Some((child, checked)) =
                lower_helper_expr(context, input, module, def, &args[0], typed)?
            else {
                return Ok(None);
            };
            if let Some(((helper_module, helper_def), helper_args)) =
                helper_application(tc, module, &args[1])
            {
                return apply_helper(
                    context,
                    input,
                    module,
                    def,
                    body.id,
                    child,
                    checked,
                    helper_module,
                    helper_def,
                    0,
                    &helper_args,
                    body,
                    typed,
                );
            }
            if let Some(stage_expr) = inline_stage_expr(&args[1]) {
                return apply_plain_stage_expr(
                    input,
                    module,
                    def,
                    &stage_expr,
                    child,
                    checked,
                    typed,
                );
            }
            return Ok(None);
        }
    }

    Ok(None)
}

fn apply_helper(
    context: &mut LoweringContext,
    input: CompilerInput<'_>,
    use_module: usize,
    use_def: usize,
    use_site: u32,
    child: NodeId,
    checked: CheckedQuery,
    helper_module: usize,
    helper_def: usize,
    query_param_index: usize,
    helper_args: &[Expr],
    use_expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let mut helper_stages: Vec<_> = tc.row_stages(helper_module, helper_def).cloned().collect();
    if helper_stages.is_empty()
        || helper_stages.iter().any(|stage| {
            !matches!(
                stage.kind,
                RowStage::Prefix
                    | RowStage::Suffix
                    | RowStage::Omit
                    | RowStage::MapValue
                    | RowStage::Where
                    | RowStage::Order
                    | RowStage::Limit
                    | RowStage::Offset
                    | RowStage::Distinct
                    | RowStage::Select
                    | RowStage::Update
                    | RowStage::Agg
            )
        })
    {
        return Ok(None);
    }
    let helper_syntax = &input.module(helper_module).def(helper_def).body;
    // The checker records stages by source visitation. Nested direct calls
    // write the outer application first even though the inner application runs
    // first, so order this helper's stages by their AST nesting depth.
    helper_stages.sort_by_key(|stage| {
        std::cmp::Reverse(expr_depth_to_id(helper_syntax, stage.site, 0).unwrap_or(0))
    });
    let params = lambda_params(helper_syntax);
    if params.iter().collect::<HashSet<_>>().len() != params.len() {
        return Ok(None);
    }

    // A helper body is checked once with an open query parameter. At a use
    // site that parameter must denote the caller's concrete typed graph before
    // we walk the body; otherwise a join or a stage after the parameter has no
    // relational child to consume. Keep the binding local to this attempt so
    // nested helper calls cannot leak it into another definition.
    let helper_tail = lambda_tail(helper_syntax);
    let has_combine = contains_combine_tree(input, helper_module, helper_tail)
        || contains_join_operator_tree(helper_tail);
    if has_combine {
        if let Some(parameter) = params.get(query_param_index) {
            let bound = typed.clone().finish(child);
            let mut syntax = HashMap::new();
            index_exprs(helper_tail, &mut syntax);
            for expr in syntax.values() {
                if matches!(&expr.kind, ExprKind::Name(name) if name == parameter) {
                    context
                        .query_bindings
                        .insert((helper_module, expr.id), (bound.clone(), checked.clone()));
                }
            }
            let mut body_typed = TypedQuery::new();
            let composed = lower_plain_composed_expr(
                context,
                input,
                helper_module,
                helper_def,
                helper_tail,
                &mut body_typed,
            )?;
            for expr in syntax.values() {
                if matches!(&expr.kind, ExprKind::Name(name) if name == parameter) {
                    context.query_bindings.remove(&(helper_module, expr.id));
                }
            }
            if let Some((root, body_checked)) = composed {
                let copied = copy_typed_graph(&body_typed.finish(root), typed)?;
                let root = copied;
                return Ok(Some((root, body_checked)));
            }
        }
    }
    let plain = apply_plain_helper_pipeline(
        input,
        use_module,
        use_def,
        use_site,
        child,
        checked.clone(),
        helper_module,
        helper_def,
        helper_args,
        use_expr,
        &helper_stages,
        typed,
    )?;
    if let Some(lowered) = plain {
        return Ok(Some(lowered));
    }
    // Unsupported helper payloads decline to the single route boundary.
    return Ok(None);
}

/// Instantiate a helper when every stage payload is in the plain checked
/// expression subset. The helper's source nodes keep their identities while
/// the caller's concrete row is threaded through the whole stage chain.
///
/// This is deliberately transactional: a failed plain lowering must leave the
/// caller's typed arena untouched so the route coordinator can decline the
/// helper without retaining partially appended nodes.
fn apply_plain_helper_pipeline(
    input: CompilerInput<'_>,
    use_module: usize,
    _use_def: usize,
    use_site: u32,
    child: NodeId,
    mut checked: CheckedQuery,
    helper_module: usize,
    helper_def: usize,
    helper_args: &[Expr],
    _use_expr: &Expr,
    stages: &[crate::check::RowStageView],
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    // Projection helper chains may change the inferred type of an expression
    // between open helper rows (for example `upper = .name` after an open
    // `select`). Keep those on the established checked payload route until the
    // checker exposes per-use intermediate projection types directly.
    let helper_body = &input.module(helper_module).def(helper_def).body;
    let mut syntax = HashMap::new();
    index_exprs(helper_body, &mut syntax);
    let params = lambda_params(helper_body);

    // Build on a clone so any unsupported expression can cleanly decline
    // without retaining partially appended nodes.
    let mut candidate = typed.clone();
    let mut current = child;
    for stage in stages {
        let Some(stage_expr) = find_expr_id(helper_body, stage.site) else {
            return Ok(None);
        };
        let ExprKind::App(_, args) = &stage_expr.kind else {
            return Ok(None);
        };
        let origin = Origin::new(helper_module, stage_expr.span);
        let input_row = checked.row().clone();
        if stage
            .input
            .as_deref()
            .is_some_and(|expected| expected != input_row.columns())
        {
            return Ok(None);
        }

        let mut predicate = None;
        let mut order_keys = None;
        let mut projection_fields = None;
        let mut label = None;
        let mut count = None;
        match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
                label = stage
                    .label
                    .clone()
                    .or_else(|| helper_witness(stage, &syntax, &params, helper_args));
                if label.is_none() {
                    return Ok(None);
                }
            }
            RowStage::Where => {
                let Some(expr) = args.first() else {
                    return Ok(None);
                };
                predicate =
                    lower_plain_expr_with_row(input, helper_module, expr, Some(&input_row))?;
                if predicate.is_none() {
                    return Ok(None);
                }
            }
            RowStage::Order => {
                let Some(list) = args.first() else {
                    return Ok(None);
                };
                let ExprKind::List(items) = &list.kind else {
                    return Ok(None);
                };
                let mut keys = Vec::with_capacity(items.len());
                for item in items {
                    let ExprKind::App(_, item_args) = &item.kind else {
                        return Ok(None);
                    };
                    let Some(value) = item_args.first() else {
                        return Ok(None);
                    };
                    let Some(direction) = sort_direction(input, helper_module, item) else {
                        return Ok(None);
                    };
                    let Some(value) =
                        lower_plain_expr_with_row(input, helper_module, value, Some(&input_row))?
                    else {
                        return Ok(None);
                    };
                    keys.push((value, direction));
                }
                order_keys = Some(keys);
            }
            RowStage::Select | RowStage::Update => {
                let Some(record) = args.first() else {
                    return Ok(None);
                };
                let ExprKind::Record(source_fields) = &record.kind else {
                    return Ok(None);
                };
                let mut fields = Vec::with_capacity(source_fields.len());
                for (name, value) in source_fields {
                    let Some(value) =
                        lower_plain_expr_with_row(input, helper_module, value, Some(&input_row))?
                    else {
                        return Ok(None);
                    };
                    fields.push((name.clone(), value));
                }
                if stage.kind == RowStage::Agg {
                    if let Some(output) = stage.output.as_deref() {
                        fields.sort_by_key(|(name, _)| {
                            output
                                .iter()
                                .position(|(column, _)| column == name)
                                .unwrap_or(usize::MAX)
                        });
                    }
                }
                projection_fields = Some(fields);
            }
            RowStage::Limit | RowStage::Offset => {
                count = syntax.get(&stage.site).and_then(stage_count);
                if count.is_none() {
                    return Ok(None);
                }
            }
            RowStage::Distinct => {}
            RowStage::Agg => {
                let Some(record) = args.first() else {
                    return Ok(None);
                };
                let ExprKind::Record(source_fields) = &record.kind else {
                    return Ok(None);
                };
                let mut fields = Vec::with_capacity(source_fields.len());
                for (name, value) in source_fields {
                    let Some(value) =
                        lower_plain_expr_with_row(input, helper_module, value, Some(&input_row))?
                    else {
                        return Ok(None);
                    };
                    fields.push((name.clone(), value));
                }
                projection_fields = Some(fields);
            }
        }

        checked = match stage.kind {
            RowStage::Prefix => CheckedQuery::prefix(checked, label.expect("label"), origin),
            RowStage::Suffix => CheckedQuery::suffix(checked, label.expect("label"), origin),
            RowStage::Omit => CheckedQuery::omit(checked, label.expect("label"), origin),
            RowStage::MapValue => CheckedQuery::map_value(checked, label.expect("label"), origin),
            RowStage::Where => {
                CheckedQuery::where_(checked, predicate.clone().expect("predicate"), origin)
            }
            RowStage::Order => {
                CheckedQuery::order(checked, order_keys.clone().expect("order keys"), origin)
            }
            RowStage::Limit => CheckedQuery::limit(checked, count.expect("count"), origin),
            RowStage::Offset => CheckedQuery::offset(checked, count.expect("count"), origin),
            RowStage::Distinct => CheckedQuery::distinct(checked, origin),
            RowStage::Select => {
                CheckedQuery::select(checked, projection_fields.clone().expect("fields"), origin)
            }
            RowStage::Update => {
                CheckedQuery::update(checked, projection_fields.clone().expect("fields"), origin)
            }
            RowStage::Agg => {
                CheckedQuery::agg(checked, projection_fields.clone().expect("fields"), origin)
            }
        }
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
        if stage
            .output
            .as_deref()
            .is_some_and(|expected| expected != checked.row().columns())
        {
            return Ok(None);
        }
        let output_row = Row::new(checked.row().columns().to_vec());
        current = match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => candidate
                .rewrite(
                    stage.node,
                    match stage.kind {
                        RowStage::Prefix => QueryNodeKind::Prefix,
                        RowStage::Suffix => QueryNodeKind::Suffix,
                        RowStage::Omit => QueryNodeKind::Omit,
                        RowStage::MapValue => QueryNodeKind::MapValue,
                        _ => unreachable!(),
                    },
                    current,
                    Some(Row::new(input_row.columns().to_vec())),
                    Some(output_row),
                    origin,
                )
                .map_err(query_error)?,
            RowStage::Where | RowStage::Order => candidate
                .keep_row_with_payload(
                    stage.node,
                    if stage.kind == RowStage::Where {
                        QueryNodeKind::Where
                    } else {
                        QueryNodeKind::Order
                    },
                    current,
                    origin,
                    predicate,
                    order_keys,
                    None,
                    None,
                )
                .map_err(query_error)?,
            RowStage::Limit | RowStage::Offset | RowStage::Distinct => {
                typed_keep_stage(&mut candidate, current, stage.kind, stage.node, origin)?
            }
            RowStage::Select | RowStage::Update | RowStage::Agg => candidate
                .project(
                    stage.node,
                    match stage.kind {
                        RowStage::Select => QueryNodeKind::Select,
                        RowStage::Update => QueryNodeKind::Update,
                        RowStage::Agg => QueryNodeKind::Agg,
                        _ => unreachable!(),
                    },
                    current,
                    Some(output_row),
                    origin,
                    projection_fields,
                )
                .map_err(query_error)?,
        };
    }

    let expected = input
        .type_check()
        .typed_node(use_module, use_site)
        .and_then(|node| node.row());
    if expected.is_some()
        && !unordered_rows_match(expected.as_deref(), Some(checked.row().columns()))
    {
        return Ok(None);
    }
    *typed = candidate;
    Ok(Some((current, checked)))
}

fn checked_kind_matches(kind: crate::checked::QueryKind, stage: RowStage) -> bool {
    matches!(
        (kind, stage),
        (crate::checked::QueryKind::Where, RowStage::Where)
            | (crate::checked::QueryKind::Select, RowStage::Select)
            | (crate::checked::QueryKind::Update, RowStage::Update)
            | (crate::checked::QueryKind::Agg, RowStage::Agg)
            | (crate::checked::QueryKind::Order, RowStage::Order)
            | (crate::checked::QueryKind::Limit, RowStage::Limit)
            | (crate::checked::QueryKind::Offset, RowStage::Offset)
            | (crate::checked::QueryKind::Distinct, RowStage::Distinct)
            | (
                crate::checked::QueryKind::Rename,
                RowStage::Prefix | RowStage::Suffix
            )
            | (crate::checked::QueryKind::Omit, RowStage::Omit)
            | (crate::checked::QueryKind::MapValue, RowStage::MapValue)
    )
}

fn helper_witness(
    stage: &crate::check::RowStageView,
    syntax: &HashMap<u32, &Expr>,
    params: &[String],
    helper_args: &[Expr],
) -> Option<String> {
    let stage_expr = syntax.get(&stage.site)?;
    let ExprKind::App(_, stage_args) = &stage_expr.kind else {
        return None;
    };
    let Some(ExprKind::Name(parameter)) = stage_args.first().map(|arg| &arg.kind) else {
        return None;
    };
    let position = params.iter().position(|name| name == parameter)?;
    string_literal(helper_args.get(position)?)
}

fn copy_typed_chain(source: &TypedQuery, target: &mut TypedQuery) -> Result<NodeId, Error> {
    // A typed chain can contain row-preserving and projection nodes as well as
    // rewrites. Use the graph copier so each kind carries its own payload and
    // constructor rather than treating every non-table node as a rewrite.
    copy_typed_graph(source, target)
}

/// Regression comparison between typed and legacy lowering routes.
#[cfg(test)]
pub(crate) fn compare_with_terms(
    input: CompilerInput<'_>,
    typed: &TypedQuery,
    checked: &CheckedQuery,
    terms: &CheckedQuery,
) -> Result<(), Error> {
    let fail = |message: &str| Error::unsupported(message).at(checked.origin());
    if !rows_equivalent(checked.row().columns(), terms.row().columns()) {
        return Err(fail("typed and term query routes produced different rows"));
    }
    let typed_rel = crate::checked::erase(checked.clone())
        .map_err(|_| fail("typed query could not be erased during route comparison"))?;
    let term_rel = crate::checked::erase(terms.clone())
        .map_err(|_| fail("term query could not be erased during route comparison"))?;
    if !relations_match(&typed_rel, &term_rel) {
        return Err(Error::unsupported(format!(
            "typed and term query routes produced different relations: typed={typed_rel:?}, terms={term_rel:?}"
        ))
        .at(checked.origin()));
    }

    let mut matched = 0usize;
    // A combine has two query subtrees. Walk the whole arena rather than only
    // following the first input, otherwise stages inside the right operand
    // would never be checked against their recorded identities.
    for node in typed
        .nodes()
        .filter(|node| node.kind() != QueryNodeKind::Table)
    {
        if matches!(node.kind(), QueryNodeKind::Join(_) | QueryNodeKind::Set(_)) {
            let Some(combine) = input
                .type_check()
                .typed_node(node.site().definition_module, node.site().expr_id)
                .and_then(|typed| typed.combine.as_ref())
            else {
                return Err(fail("typed query route contains an unrecorded combine"));
            };
            let expected_kind = match combine.kind {
                CombineKind::Join(kind) => QueryNodeKind::Join(kind),
                CombineKind::Set(kind) => QueryNodeKind::Set(kind),
            };
            let actual_inputs: Vec<_> =
                node.input_rows().map(|row| row.map(Row::columns)).collect();
            let expected_inputs: Vec<_> = combine.inputs.iter().map(Option::as_deref).collect();
            let inputs_match = match combine.kind {
                // Query rows are named, so inference may close a join input in
                // a different field order than the relation child preserves.
                // Set operands remain positional and therefore require the
                // exact order.
                CombineKind::Join(_) => {
                    actual_inputs.len() == expected_inputs.len()
                        && actual_inputs
                            .iter()
                            .zip(expected_inputs.iter())
                            .all(|(actual, expected)| unordered_rows_match(*actual, *expected))
                }
                CombineKind::Set(_) => actual_inputs == expected_inputs,
            };
            if node.site() != combine.node
                || node.kind() != expected_kind
                || !inputs_match
                || node.output_row().map(Row::columns) != combine.output.as_deref()
            {
                return Err(Error::unsupported(format!(
                    "typed and checker combine records disagree: site={:?}/{:?}, kind={:?}/{:?}, inputs={actual_inputs:?}/{expected_inputs:?}, output={:?}/{:?}",
                    node.site(),
                    combine.node,
                    node.kind(),
                    expected_kind,
                    node.output_row().map(Row::columns),
                    combine.output.as_deref(),
                ))
                .at(node.origin()));
            }
            continue;
        }
        let Some(stage) = input
            .type_check()
            .row_stages(node.site().definition_module, node.site().definition)
            .find(|stage| stage.node == node.site())
        else {
            if matches!(
                node.kind(),
                QueryNodeKind::Limit | QueryNodeKind::Offset | QueryNodeKind::Distinct
            ) {
                continue;
            }
            return Err(fail("typed query route contains an unrecorded stage"));
        };
        let mut syntax = HashMap::new();
        index_exprs(
            &input
                .module(node.site().definition_module)
                .def(node.site().definition)
                .body,
            &mut syntax,
        );
        let (expected_kind, expected_input, expected_output) = match stage.kind {
            RowStage::Prefix => (
                QueryNodeKind::Prefix,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Suffix => (
                QueryNodeKind::Suffix,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Omit => (
                QueryNodeKind::Omit,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::MapValue => (
                QueryNodeKind::MapValue,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Where => (
                QueryNodeKind::Where,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Select => (
                QueryNodeKind::Select,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Update => (
                QueryNodeKind::Update,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Agg => (
                QueryNodeKind::Agg,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Order => (
                QueryNodeKind::Order,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Limit => (
                QueryNodeKind::Limit,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Offset => (
                QueryNodeKind::Offset,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
            RowStage::Distinct => (
                QueryNodeKind::Distinct,
                stage.input.as_deref(),
                stage.output.as_deref(),
            ),
        };
        let input_disagrees = !rows_match(
            stage.kind,
            expected_input,
            node.input_row().map(|row| row.columns()),
        );
        let output_disagrees = !rows_match(
            stage.kind,
            expected_output,
            node.output_row().map(|row| row.columns()),
        );
        if node.site() != stage.node
            || node.kind() != expected_kind
            || node.origin().module != node.site().definition_module
            || syntax
                .get(&stage.node.expr_id)
                .is_none_or(|expr| node.origin().span != expr.span)
            || input_disagrees
            || output_disagrees
        {
            return Err(Error::unsupported(format!(
                "typed and checker stage records disagree at {:?}",
                stage.node
            ))
            .at(node.origin()));
        }
        matched += 1;
    }
    let expected = typed
        .nodes()
        .filter(|node| node.kind() != QueryNodeKind::Table)
        .filter(|node| {
            input
                .type_check()
                .row_stages(node.site().definition_module, node.site().definition)
                .any(|stage| stage.node == node.site())
        })
        .count();
    if matched != expected {
        return Err(fail(
            "typed query route and checker stage records have different sizes",
        ));
    }
    Ok(())
}

fn unordered_rows_match(
    actual: Option<&[(String, crate::core::ScalarType)]>,
    expected: Option<&[(String, crate::core::ScalarType)]>,
) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => {
            let mut actual = actual.to_vec();
            let mut expected = expected.to_vec();
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual == expected
        }
        _ => false,
    }
}

/// The normalized adapter may retain `Unknown` for a polymorphic helper's
/// intermediate field even after the typed checker has instantiated the same
/// field at a concrete use site. Treat that adapter placeholder as a wildcard
/// during the migration comparison; concrete disagreements remain failures.
#[cfg(test)]
fn rows_equivalent(
    actual: &[(String, crate::core::ScalarType)],
    expected: &[(String, crate::core::ScalarType)],
) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|((an, at), (en, et))| {
            an == en
                && (at == et
                    || matches!(at, crate::core::ScalarType::Unknown)
                    || matches!(et, crate::core::ScalarType::Unknown))
        })
}

#[cfg(test)]
fn relations_match(left: &crate::ir::Rel, right: &crate::ir::Rel) -> bool {
    use crate::ir::Rel;
    match (left, right) {
        (Rel::At(_, l), r) => relations_match(l, r),
        (l, Rel::At(_, r)) => relations_match(l, r),
        (
            Rel::Table {
                schema: ls,
                name: ln,
                columns: lc,
            },
            Rel::Table {
                schema: rs,
                name: rn,
                columns: rc,
            },
        ) => ls == rs && ln == rn && lc == rc,
        (Rel::Omit(li, lk), Rel::Omit(ri, rk))
        | (Rel::Prefix(li, lk), Rel::Prefix(ri, rk))
        | (Rel::Suffix(li, lk), Rel::Suffix(ri, rk)) => lk == rk && relations_match(li, ri),
        (Rel::Limit(li, ln), Rel::Limit(ri, rn)) | (Rel::Offset(li, ln), Rel::Offset(ri, rn)) => {
            ln == rn && relations_match(li, ri)
        }
        (Rel::Distinct(li), Rel::Distinct(ri)) => relations_match(li, ri),
        (Rel::Where(li, lp), Rel::Where(ri, rp)) => lp == rp && relations_match(li, ri),
        (Rel::Select(li, lp), Rel::Select(ri, rp))
        | (Rel::Update(li, lp), Rel::Update(ri, rp))
        | (Rel::Agg(li, lp), Rel::Agg(ri, rp)) => lp == rp && relations_match(li, ri),
        (Rel::Order(li, lp), Rel::Order(ri, rp)) => lp == rp && relations_match(li, ri),
        (
            Rel::Join {
                kind: lk,
                left: ll,
                right: lr,
                on: lo,
            },
            Rel::Join {
                kind: rk,
                left: rl,
                right: rr,
                on: ro,
            },
        ) => lk == rk && lo == ro && relations_match(ll, rl) && relations_match(lr, rr),
        (
            Rel::Set {
                kind: lk,
                left: ll,
                right: lr,
            },
            Rel::Set {
                kind: rk,
                left: rl,
                right: rr,
            },
        ) => lk == rk && relations_match(ll, rl) && relations_match(lr, rr),
        _ => left == right,
    }
}

fn query_target(
    tc: &crate::check::TypeCheck,
    module: usize,
    expr: &Expr,
) -> Option<(usize, usize)> {
    match &expr.kind {
        ExprKind::Name(_) | ExprKind::Proj(_, _) => match tc.resolution(module, expr.id) {
            Some(Resolved::Def(target)) => Some(*target),
            _ => None,
        },
        _ => None,
    }
}

fn helper_application(
    tc: &crate::check::TypeCheck,
    module: usize,
    expr: &Expr,
) -> Option<((usize, usize), Vec<Expr>)> {
    let (head, args) = match &expr.kind {
        ExprKind::App(head, args) => (&**head, args.to_vec()),
        _ => (expr, Vec::new()),
    };
    let target = match tc.resolution(module, head.id) {
        Some(Resolved::Def(target)) => *target,
        _ => {
            let mut choices = tc.typed_node(module, head.id)?.choices.iter().filter_map(
                |(_, choice)| match choice {
                    crate::check::Choice::Def(module, def) => Some((*module, *def)),
                    crate::check::Choice::Hole(_) => None,
                },
            );
            let target = choices.next()?;
            if choices.any(|choice| choice != target) {
                return None;
            }
            target
        }
    };
    Some((target, args))
}

/// Extract the stage call from an inline stage lambda such as
/// `x => where (.age > 1) x`. The final parameter application is a value-level
/// encoding of the pipeline input and is omitted from the stage payload.
fn inline_stage_expr(expr: &Expr) -> Option<Expr> {
    let ExprKind::Lambda(parameter, body) = &expr.kind else {
        return None;
    };
    let ExprKind::App(head, args) = &body.kind else {
        return None;
    };
    let Some(last) = args.last() else {
        return None;
    };
    if !matches!(&last.kind, ExprKind::Name(name) if name == parameter) {
        return None;
    }
    Some(Expr {
        id: body.id,
        span: body.span,
        kind: ExprKind::App(head.clone(), args[..args.len() - 1].to_vec().into()),
    })
}

/// Evaluate an inline stage lambda against an already lowered query. The
/// surface form is curried application, so a lambda such as
/// `x => select {...} (where (...) x)` contains two stage applications whose
/// query argument is the final argument of each application.
fn lower_inline_stage_body(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    parameter: &str,
    body: &Expr,
    child: NodeId,
    checked: CheckedQuery,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    if matches!(&body.kind, ExprKind::Name(name) if name == parameter) {
        return Ok(Some((child, checked)));
    }
    let ExprKind::App(head, args) = &body.kind else {
        return Ok(None);
    };
    let Some(query_arg) = args.last() else {
        return Ok(None);
    };
    let Some((query_child, query_checked)) = lower_inline_stage_body(
        input, module, def, parameter, query_arg, child, checked, typed,
    )?
    else {
        return Ok(None);
    };
    let stage = Expr {
        id: body.id,
        span: body.span,
        kind: ExprKind::App(
            head.clone(),
            args[..args.len() - 1].to_vec().into_boxed_slice(),
        ),
    };
    let result = apply_plain_stage_expr(
        input,
        module,
        def,
        &stage,
        query_child,
        query_checked,
        typed,
    );
    result
}

fn lambda_params(expr: &Expr) -> Vec<String> {
    let mut params = Vec::new();
    let mut body = expr;
    while let ExprKind::Lambda(parameter, next) = &body.kind {
        params.push(parameter.clone());
        body = next;
    }
    params
}

fn lambda_tail(expr: &Expr) -> &Expr {
    let mut body = expr;
    while let ExprKind::Lambda(_, next) = &body.kind {
        body = next;
    }
    body
}

fn is_pipe_head(head: &Expr) -> bool {
    matches!(&head.kind, ExprKind::Name(name) if name == "_&_")
}

fn is_stage_shorthand_head(head: &Expr) -> bool {
    matches!(
        &head.kind,
        ExprKind::Name(name)
            if matches!(
                name.as_str(),
                "_&?_" | "_&=_" | "_&+_" | "_&*_" | "_&._" | "_&-_"
            )
    )
}

fn join_operator_kind(expr: &Expr) -> Option<crate::ir::JoinKind> {
    let ExprKind::App(head, _) = &expr.kind else {
        return None;
    };
    let ExprKind::Name(name) = &head.kind else {
        return None;
    };
    match name.as_str() {
        "_?_" => Some(crate::ir::JoinKind::Inner),
        "_<?_" => Some(crate::ir::JoinKind::Left),
        "_?>_" => Some(crate::ir::JoinKind::Right),
        "_<??>_" => Some(crate::ir::JoinKind::Full),
        _ => None,
    }
}

fn contains_join_operator_tree(expr: &Expr) -> bool {
    join_operator_kind(expr).is_some()
        || crate::check::child_exprs(expr)
            .into_iter()
            .any(contains_join_operator_tree)
}

fn contains_unmigrated_stage(input: CompilerInput<'_>, module: usize, body: &Expr) -> bool {
    let tc = input.type_check();
    let mut stack = vec![body];
    while let Some(expr) = stack.pop() {
        if let ExprKind::Name(_) | ExprKind::Proj(_, _) = &expr.kind {
            if let Some(Resolved::Def((m, d))) = tc.resolution(module, expr.id) {
                let target = input.module(*m).def(*d);
                if let ExprKind::Primitive(name) = &target.body.kind {
                    if let Some(prim) = crate::primitive::prim_of(name) {
                        let Some(stage) = crate::check::RowStage::of_prim(prim) else {
                            stack.extend(crate::check::child_exprs(expr));
                            continue;
                        };
                        if !matches!(
                            stage,
                            RowStage::Prefix
                                | RowStage::Suffix
                                | RowStage::Omit
                                | RowStage::MapValue
                        ) {
                            return true;
                        }
                    }
                } else if tc.row_stages(*m, *d).next().is_some() {
                    // A helper that applies a stage is a composition slice;
                    // its stages belong to the helper's tree and need the
                    // later instantiation step.
                    return true;
                }
            }
        }
        stack.extend(crate::check::child_exprs(expr));
    }
    false
}

fn query_error(error: crate::check::QueryErr) -> Error {
    Error::unsupported(error.message).at(error.origin)
}

fn index_exprs<'a>(expr: &'a Expr, out: &mut HashMap<u32, &'a Expr>) {
    out.insert(expr.id, expr);
    for child in crate::check::child_exprs(expr) {
        index_exprs(child, out);
    }
}

/// Instantiate a helper body with the concrete expressions supplied at a use
/// site. The checker has already resolved those arguments; preserving the
/// helper's surrounding node ids lets typed lowering continue to read the
/// helper's recorded stage identities while replacing only parameter leaves.
fn substitute_expr(expr: &Expr, substitutions: &HashMap<&str, Expr>) -> Expr {
    if let ExprKind::Name(name) = &expr.kind {
        if let Some(replacement) = substitutions.get(name.as_str()) {
            return replacement.clone();
        }
    }
    let kind = match &expr.kind {
        ExprKind::Name(name) => ExprKind::Name(name.clone()),
        ExprKind::Lit(lit) => ExprKind::Lit(lit.clone()),
        ExprKind::Field(side, name) => ExprKind::Field(*side, name.clone()),
        ExprKind::Proj(base, name) => {
            ExprKind::Proj(Box::new(substitute_expr(base, substitutions)), name.clone())
        }
        ExprKind::App(head, args) => ExprKind::App(
            Box::new(substitute_expr(head, substitutions)),
            args.iter()
                .map(|arg| substitute_expr(arg, substitutions))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
        ExprKind::Lambda(parameter, body) => {
            if substitutions.contains_key(parameter.as_str()) {
                ExprKind::Lambda(parameter.clone(), body.clone())
            } else {
                ExprKind::Lambda(
                    parameter.clone(),
                    Box::new(substitute_expr(body, substitutions)),
                )
            }
        }
        ExprKind::Record(fields) => ExprKind::Record(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), substitute_expr(value, substitutions)))
                .collect(),
        ),
        ExprKind::List(items) => ExprKind::List(
            items
                .iter()
                .map(|item| substitute_expr(item, substitutions))
                .collect(),
        ),
        ExprKind::Sql(sql) => ExprKind::Sql(sql.clone()),
        ExprKind::Case(arms) => ExprKind::Case(
            arms.iter()
                .map(|(condition, value)| {
                    (
                        substitute_expr(condition, substitutions),
                        substitute_expr(value, substitutions),
                    )
                })
                .collect(),
        ),
        ExprKind::Primitive(name) => ExprKind::Primitive(name.clone()),
        ExprKind::Error => ExprKind::Error,
    };
    Expr {
        id: expr.id,
        span: expr.span,
        kind,
    }
}

fn stage_count(expr: &&Expr) -> Option<i64> {
    let ExprKind::App(_, args) = &expr.kind else {
        return None;
    };
    match args.first().map(|arg| &arg.kind) {
        Some(ExprKind::Lit(Lit::Int(count))) => Some(*count),
        _ => None,
    }
}

fn find_base<'a>(
    expr: &'a Expr,
    stages: &HashSet<u32>,
    tc: &crate::check::TypeCheck,
    module: usize,
) -> Option<&'a Expr> {
    if stages.contains(&expr.id) {
        return None;
    }
    let is_query = tc
        .use_scheme(module, expr.id)
        .is_some_and(|view| matches!(view, crate::check::SchemeView::Query { .. }));
    if is_query {
        let has_query_child = crate::check::child_exprs(expr).into_iter().any(|child| {
            tc.use_scheme(module, child.id)
                .is_some_and(|view| matches!(view, crate::check::SchemeView::Query { .. }))
        });
        if !has_query_child {
            return Some(expr);
        }
    }
    for child in crate::check::child_exprs(expr) {
        if let Some(found) = find_base(child, stages, tc, module) {
            return Some(found);
        }
    }
    None
}

fn table_parts(
    input: CompilerInput<'_>,
    module: usize,
    expr: &Expr,
) -> Option<(String, String, Vec<(String, crate::core::ScalarType)>)> {
    fn visit(
        input: CompilerInput<'_>,
        module: usize,
        expr: &Expr,
        seen: &mut HashSet<(usize, usize)>,
    ) -> Option<(String, String, Vec<(String, crate::core::ScalarType)>)> {
        let tc = input.type_check();
        match &expr.kind {
            ExprKind::App(head, args) if args.len() == 2 && is_table_head(input, module, head) => {
                let schema = string_literal(&args[0])?;
                let table = string_literal(&args[1])?;
                let columns = tc.typed_node(module, expr.id)?.row()?;
                Some((schema, table, columns))
            }
            ExprKind::Name(_) | ExprKind::Proj(_, _) => {
                let Resolved::Def((target_module, target_def)) =
                    tc.resolution(module, expr.id)?.clone()
                else {
                    return None;
                };
                if !seen.insert((target_module, target_def)) {
                    return None;
                }
                let target = input.module(target_module).def(target_def);
                let (schema, table, _) = visit(input, target_module, &target.body, seen)?;
                let columns = tc.scheme_fields(target_module, target_def)?;
                Some((schema, table, columns))
            }
            _ => None,
        }
    }

    visit(input, module, expr, &mut HashSet::new())
}

fn is_table_head(input: CompilerInput<'_>, module: usize, head: &Expr) -> bool {
    if matches!(&head.kind, ExprKind::Primitive(name) if name == "__table") {
        return true;
    }
    let Some(Resolved::Def((m, d))) = input.type_check().resolution(module, head.id) else {
        return false;
    };
    matches!(&input.module(*m).def(*d).body.kind, ExprKind::Primitive(name) if name == "__table")
}

fn string_literal(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Lit(Lit::Str(value)) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check;
    use crate::workspace::Workspace;

    fn lower_source(src: &str, name: &str) -> (TypedQuery, CheckedQuery) {
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .expect("definition");
        let body = &ws.modules[ws.root].module.defs[def].body;
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(input, ws.root, def, body)
            .expect("typed lowering")
            .expect("supported direct pipeline");
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body)
            .expect("term lowering");
        compare_with_terms(input, &typed, &checked, &terms).expect("typed route agrees");
        (typed, checked)
    }

    #[test]
    fn direct_rewrites_build_a_typed_chain_and_checked_query() {
        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             q = users & prefix \"u_\" & suffix \"_v2\"\n",
            "q",
        );
        let chain = typed.chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(typed.node(chain[0]).kind(), QueryNodeKind::Suffix);
        assert_eq!(typed.node(chain[1]).kind(), QueryNodeKind::Prefix);
        assert_eq!(typed.node(chain[2]).kind(), QueryNodeKind::Table);
        assert_eq!(
            checked.row().columns(),
            &[
                ("u_id_v2".to_string(), crate::core::ScalarType::Int),
                ("u_name_v2".to_string(), crate::core::ScalarType::String),
            ]
        );
    }

    #[test]
    fn direct_expression_stages_use_the_typed_route() {
        let ws = Workspace::from_source(
            "users : query { id = int, name = string, age = int } = table \"s\" \"users\"\nq = users & where (.age > 1) & update { older = .age } & select { .name, .older } & order [asc .name]\n",
        );
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        assert!(matches!(
            crate::query_lowering::lower(
                input,
                ws.root,
                def,
                &ws.modules[ws.root].module.defs[def].body
            )
            .unwrap(),
            crate::query_lowering::LoweredQuery::Typed { .. }
        ));
    }

    #[test]
    fn direct_plain_select_bypasses_normalized_elaboration() {
        let ws = Workspace::from_source(
            "spec = { order = [asc .id] }\n\
             users : query { id = int, name = string } = table \"s\" \"users\"\n\
             q = users & select { id = .id, name = .name, label = ifThenElse (.id > 0) .name \"unknown\", id_text = toString .id, has_u = contains \"u\" .name, rn = rowNumber spec }\n",
        );
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|definition| definition.name == "q")
            .unwrap();
        crate::normalize::clear_visited();
        let result = lower(
            CompilerInput::new(&ws, &tc),
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.0.root_kind(), QueryNodeKind::Select);
        assert!(
            crate::normalize::visited_nodes().is_empty(),
            "simple select unexpectedly entered normalized elaboration"
        );
        let node = result.0.node(result.0.root());
        let fields = node.projection_fields().expect("typed select has fields");
        assert_eq!(fields[2].0, "label");
        assert_eq!(fields[2].1.kind(), crate::checked::ExprKind::Template);
        assert_eq!(fields[3].0, "id_text");
        assert_eq!(fields[3].1.kind(), crate::checked::ExprKind::Template);
        assert_eq!(fields[4].0, "has_u");
        assert_eq!(fields[4].1.kind(), crate::checked::ExprKind::Template);
        assert_eq!(fields[5].0, "rn");
        assert_eq!(fields[5].1.kind(), crate::checked::ExprKind::WinTemplate);
    }

    #[test]
    fn direct_plain_date_and_decimal_templates_build_typed_fields() {
        let (typed, checked) = lower_source(
            "users : query { id = int, created = date, created_at = timestamp } = table \"s\" \"users\"\n\
             q = users & select { month = month .created, added = addDays 1 .created, \
             truncated = truncMonth .created_at, decimal = toDecimal 18 2 .id }\n",
            "q",
        );
        let node = typed.node(typed.root());
        let fields = node.projection_fields().expect("typed select has fields");
        assert_eq!(fields.len(), 4);
        assert!(fields
            .iter()
            .all(|(_, field)| field.kind() == crate::checked::ExprKind::Template));
        assert_eq!(
            checked.row().names(),
            &["month", "added", "truncated", "decimal"]
        );
    }

    #[test]
    fn direct_plain_templates_follow_resolved_sql_definitions_and_aliases() {
        let (typed, checked) = lower_source(
            "shout : expr r string -> expr r string = sql \"UPPER($1)\"\n\
             loud : expr r string -> expr r string = shout\n\
             users : query { name = string } = table \"s\" \"users\"\n\
             q = users & select { name = loud .name }\n",
            "q",
        );
        let node = typed.node(typed.root());
        let fields = node.projection_fields().expect("typed select has fields");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].1.kind(), crate::checked::ExprKind::Template);
        assert_eq!(checked.row().names(), &["name"]);
    }

    #[test]
    fn plain_sort_keys_follow_resolved_direction_primitives() {
        let (typed, checked) = lower_source(
            "ascending : expr r a -> sortkey r = asc\n\
             descending : expr r a -> sortkey r = desc\n\
             users : query { id = int } = table \"s\" \"users\"\n\
             q = users & order [descending .id] & select { id = .id, rn = rowNumber {order = [ascending .id]} }\n",
            "q",
        );
        let order = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| node.kind() == QueryNodeKind::Order)
            .expect("typed order node");
        assert!(!order.order_keys().expect("order keys")[0].1);
        assert_eq!(checked.row().names(), &["id", "rn"]);
    }

    #[test]
    fn direct_order_lowers_supported_expression_keys_without_the_bridge() {
        let (typed, checked) = lower_source(
            "descending : expr r a -> sortkey r = desc\n\
             users : query { id = maybe int } = table \"s\" \"users\"\n\
             q = users & order [descending (coalesce 0 .id)]\n",
            "q",
        );
        let order = typed.node(typed.root());
        let key = &order.order_keys().expect("order keys")[0];
        assert!(!key.1);
        assert_eq!(key.0.kind(), crate::checked::ExprKind::Template);
        assert_eq!(checked.row().names(), &["id"]);
    }

    #[test]
    fn direct_plain_null_templates_build_typed_fields() {
        let (typed, checked) = lower_source(
            "users : query { id = int, nickname = maybe string, active = maybe bool } = table \"s\" \"users\"\n\
             q = users & select { name = coalesce \"unknown\" .nickname, \
             present = isNotNull .nickname, enabled = isTrue .active, same = eqMaybe .nickname .nickname }\n",
            "q",
        );
        let node = typed.node(typed.root());
        let fields = node.projection_fields().expect("typed select has fields");
        assert_eq!(fields.len(), 4);
        assert!(fields
            .iter()
            .all(|(_, field)| field.kind() == crate::checked::ExprKind::Template));
        assert_eq!(
            checked.row().names(),
            &["name", "present", "enabled", "same"]
        );
    }

    #[test]
    fn direct_joins_and_sets_build_two_input_typed_nodes() {
        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { id = int, uid = int } = table \"s\" \"orders\"\n\
             q = users & innerJoin orders (.<id == .>uid)\n",
            "q",
        );
        assert_eq!(
            typed.root_kind(),
            QueryNodeKind::Join(crate::ir::JoinKind::Inner)
        );
        assert_eq!(typed.node(typed.root()).inputs().len(), 2);
        assert_eq!(
            typed
                .node(typed.root())
                .join_predicate()
                .map(|predicate| predicate.kind()),
            Some(crate::checked::ExprKind::Template)
        );
        assert_eq!(checked.row().names(), &["id", "name", "uid"]);
        {
            let ws = Workspace::from_source(
                "users : query { id = int, name = string } = table \"s\" \"users\"\n\
                 orders : query { id = int, uid = int } = table \"s\" \"orders\"\n\
                 q = users & innerJoin orders (.<id == .>uid)\n",
            );
            let tc = check::check(&ws);
            let def = ws.modules[ws.root]
                .module
                .defs
                .iter()
                .position(|d| d.name == "q")
                .unwrap();
            let input = CompilerInput::new(&ws, &tc);
            let body = &ws.modules[ws.root].module.defs[def].body;
            let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
            let terms =
                crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
            compare_with_terms(input, &typed, &checked, &terms).unwrap();
        }

        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             vips : query { id = int, name = string } = table \"s\" \"vips\"\n\
             q = users & union vips\n",
            "q",
        );
        assert_eq!(
            typed.root_kind(),
            QueryNodeKind::Set(crate::ir::SetKind::Union)
        );
        assert_eq!(typed.node(typed.root()).inputs().len(), 2);
        assert_eq!(checked.row().names(), &["id", "name"]);
    }

    #[test]
    fn composed_payloads_are_routed_through_the_coordinator() {
        let ws = Workspace::from_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\norders : query { id = int, uid = int, amount = float } = table \"s\" \"orders\"\nq = users & innerJoin orders (.<id == .>uid) & where (.amount > 1.0)\n",
        );
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let lowered = crate::query_lowering::lower(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap();
        let checked = match lowered {
            crate::query_lowering::LoweredQuery::Typed { checked, .. } => checked,
        };
        assert_eq!(checked.row().names(), &["id", "name", "uid", "amount"]);
    }

    #[test]
    fn row_preserving_limit_and_offset_build_typed_nodes() {
        let ws = Workspace::from_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             q = users & limit 10 & offset 2\n",
        );
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let body = &ws.modules[ws.root].module.defs[def].body;
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        let chain = typed.chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(typed.node(chain[0]).kind(), QueryNodeKind::Offset);
        assert_eq!(typed.node(chain[1]).kind(), QueryNodeKind::Limit);
        assert_eq!(typed.node(chain[2]).kind(), QueryNodeKind::Table);
        assert_eq!(checked.kind(), crate::checked::QueryKind::Offset);
        assert_eq!(checked.row().names(), &["id", "name"]);
    }

    #[test]
    fn row_preserving_distinct_builds_a_typed_node() {
        let ws = Workspace::from_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             q = users & distinct\n",
        );
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let body = &ws.modules[ws.root].module.defs[def].body;
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(typed.root_kind(), QueryNodeKind::Distinct);
        assert_eq!(checked.kind(), crate::checked::QueryKind::Distinct);
        assert_eq!(checked.row().names(), &["id", "name"]);
    }

    #[test]
    fn map_value_keeps_its_typed_row_while_erasing_to_the_input() {
        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             q = users & mapValue \"nullable\"\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::MapValue);
        assert_eq!(
            checked.row().columns(),
            &[
                (
                    "id".to_string(),
                    crate::core::ScalarType::Maybe(Box::new(crate::core::ScalarType::Int))
                ),
                (
                    "name".to_string(),
                    crate::core::ScalarType::Maybe(Box::new(crate::core::ScalarType::String))
                ),
            ]
        );
        let rel = crate::checked::erase(checked).expect("type-only stage erases");
        assert!(
            matches!(rel, crate::ir::Rel::At(_, inner) if matches!(*inner, crate::ir::Rel::Table { .. }))
        );
    }

    #[test]
    fn query_alias_reuses_the_typed_stage_chain() {
        let (typed, checked) = lower_source(
            "users : query { id = int } = table \"s\" \"users\"\n\
             renamed = users & prefix \"u_\"\n\
             q = renamed\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Prefix);
        assert_eq!(checked.row().names(), &["u_id"]);
    }

    #[test]
    fn expression_pipeline_can_follow_a_typed_query_alias() {
        let (typed, checked) = lower_source(
            "users : query { id = int, age = int } = table \"s\" \"users\"\n\
             renamed = users & prefix \"u_\"\n\
             q = renamed & where (.u_age > 18)\n\
                 & select { id = .u_id, rn = rowNumber {order = [asc .u_age]} }\n\
                 & order [asc .id] & limit 10\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Limit);
        assert!(typed
            .nodes()
            .any(|node| node.kind() == QueryNodeKind::Prefix));
        assert!(typed
            .nodes()
            .any(|node| node.kind() == QueryNodeKind::Where));
        assert!(typed.nodes().any(|node| {
            node.kind() == QueryNodeKind::Select
                && node.projection_fields().is_some_and(|fields| {
                    fields[1].1.kind() == crate::checked::ExprKind::WinTemplate
                })
        }));
        assert_eq!(checked.row().names(), &["id", "rn"]);
    }

    #[test]
    fn projection_helper_carries_window_payloads_into_typed_nodes() {
        let (typed, checked) = lower_source(
            "users : query { id = int, age = int } = table \"s\" \"users\"\n\
             ranked = q => q & select { id = .id, rn = rowNumber {order = [asc .age]} }\n\
             q = users & ranked\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Select);
        let root = typed.node(typed.root());
        let fields = root
            .projection_fields()
            .expect("helper projection carries checked fields");
        assert_eq!(fields[1].0, "rn");
        assert_eq!(fields[1].1.kind(), crate::checked::ExprKind::WinTemplate);
        assert_eq!(checked.row().names(), &["id", "rn"]);
    }

    #[test]
    fn signed_helper_reuses_its_typed_stage_chain() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper : query { id = int } -> query { h_id = int } = q => q & prefix \"h_\"\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let (typed, checked) = lower(
            CompilerInput::new(&ws, &tc),
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        assert_eq!(typed.root_kind(), QueryNodeKind::Prefix);
        assert_eq!(checked.row().names(), &["h_id"]);
    }

    #[test]
    fn multi_projection_helper_instantiates_intermediate_rows() {
        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             helper = q => q & select { .name } & update { upper = .name }\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let q = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let body = &ws.modules[ws.root].module.defs[q].body;
        let (typed, checked) = lower(input, ws.root, q, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, q, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(
            typed
                .chain()
                .iter()
                .map(|id| typed.node(*id).kind())
                .collect::<Vec<_>>(),
            vec![
                QueryNodeKind::Update,
                QueryNodeKind::Select,
                QueryNodeKind::Table
            ]
        );
        assert_eq!(checked.row().names(), &["name", "upper"]);
        assert_eq!(
            typed.node(typed.chain()[1]).output_row().unwrap().names(),
            vec!["name"]
        );
        let chain = typed.chain();
        let select_node = typed.node(chain[1]);
        let select_fields = select_node
            .projection_fields()
            .expect("expanded select carries checked fields");
        assert_eq!(select_fields[0].1.kind(), crate::checked::ExprKind::Column);
        let update_node = typed.node(chain[0]);
        let update_fields = update_node
            .projection_fields()
            .expect("expanded update carries checked fields");
        assert_eq!(update_fields[0].0, "upper");
    }

    #[test]
    fn multi_stage_aggregate_helper_instantiates_each_output_row() {
        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             helper = q => q & select { .name } & agg { name = group .name, n = count }\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let body = &ws.modules[ws.root].module.defs[def].body;
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Agg)));
        assert!(typed
            .nodes()
            .filter(|node| matches!(node.kind(), QueryNodeKind::Select | QueryNodeKind::Agg))
            .all(|node| node.projection_fields().is_some()));
        assert_eq!(checked.row().names(), &["name", "n"]);
    }

    #[test]
    fn multi_projection_helper_keeps_row_preserving_stages_in_order() {
        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             helper = q => q & where (.id > 1) & select { .name } & limit 10\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let body = &ws.modules[ws.root].module.defs[def].body;
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(
            typed
                .chain()
                .iter()
                .map(|id| typed.node(*id).kind())
                .collect::<Vec<_>>(),
            vec![
                QueryNodeKind::Limit,
                QueryNodeKind::Select,
                QueryNodeKind::Where,
                QueryNodeKind::Table
            ]
        );
        let where_node = typed
            .chain()
            .into_iter()
            .find(|id| typed.node(*id).kind() == QueryNodeKind::Where)
            .map(|id| typed.node(id))
            .expect("expanded helper has a where node");
        assert!(where_node.predicate().is_some());
    }

    #[test]
    fn row_preserving_helper_instantiates_expression_payloads() {
        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             helper = q => q & where (.id > 1) & order [asc .name] & limit 10 & offset 2\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let body = &ws.modules[ws.root].module.defs[def].body;
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        let kinds: Vec<_> = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id).kind())
            .collect();
        assert_eq!(
            kinds,
            vec![
                QueryNodeKind::Offset,
                QueryNodeKind::Limit,
                QueryNodeKind::Order,
                QueryNodeKind::Where,
                QueryNodeKind::Table,
            ]
        );
        let where_node = typed
            .nodes()
            .find(|node| node.kind() == QueryNodeKind::Where)
            .unwrap();
        assert!(where_node.predicate().is_some());
        let order_node = typed
            .nodes()
            .find(|node| node.kind() == QueryNodeKind::Order)
            .unwrap();
        assert_eq!(order_node.order_keys().unwrap().len(), 1);
    }

    #[test]
    fn projection_helper_instantiates_rewrite_boundaries() {
        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             helper = q => q & select { .name } & prefix \"x_\" & update { upper = .x_name }\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let body = &ws.modules[ws.root].module.defs[def].body;
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        let chain: Vec<_> = typed.chain().into_iter().map(|id| typed.node(id)).collect();
        assert_eq!(
            chain.iter().map(|node| node.kind()).collect::<Vec<_>>(),
            vec![
                QueryNodeKind::Update,
                QueryNodeKind::Prefix,
                QueryNodeKind::Select,
                QueryNodeKind::Table,
            ]
        );
        assert_eq!(chain[1].input_row().unwrap().names(), vec!["name"]);
        assert_eq!(chain[1].output_row().unwrap().names(), vec!["x_name"]);
        assert_eq!(checked.row().names(), &["x_name", "upper"]);
    }

    #[test]
    fn row_polymorphic_helper_instantiates_its_use_site_row() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper = q => q & prefix \"h_\"\n\
             q = users & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(typed.root_kind(), QueryNodeKind::Prefix);
        assert_eq!(
            typed.node(typed.root()).input_row().unwrap().names(),
            &["id"]
        );
        assert_eq!(checked.row().names(), &["h_id"]);
    }

    #[test]
    fn one_open_row_helper_can_be_instantiated_repeatedly() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper = q => q & prefix \"h_\"\n\
             q = users & helper & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(typed.chain().len(), 3);
        assert_eq!(checked.row().names(), &["h_h_id"]);
    }

    #[test]
    fn open_helper_witness_comes_from_its_call_arguments() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper = affix => q => q & prefix affix\n\
             q = users & helper \"h_\"\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(checked.row().names(), &["h_id"]);
    }

    #[test]
    fn open_map_value_witness_comes_from_its_call_arguments() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper = wrapper => q => q & mapValue wrapper\n\
             q = users & helper \"nullable\"\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(
            input,
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(typed.root_kind(), QueryNodeKind::MapValue);
        assert_eq!(
            checked.row().columns(),
            &[(
                "id".to_string(),
                crate::core::ScalarType::Maybe(Box::new(crate::core::ScalarType::Int))
            )]
        );
    }

    #[test]
    fn multi_stage_helper_instantiates_each_open_row_boundary() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             helper = affix => ending => q => q & prefix affix & suffix ending\n\
             q = users & helper \"h_\" \"_v2\"\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let body = &ws.modules[ws.root].module.defs[def].body;
        let input = CompilerInput::new(&ws, &tc);
        let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
        let terms = crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
        compare_with_terms(input, &typed, &checked, &terms).unwrap();
        assert_eq!(checked.row().names(), &["h_id_v2"]);
        let chain = typed.chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(typed.node(chain[0]).kind(), QueryNodeKind::Suffix);
        assert_eq!(typed.node(chain[1]).kind(), QueryNodeKind::Prefix);
        assert_eq!(
            typed.node(chain[0]).input_row().unwrap().names(),
            vec!["h_id"]
        );
        assert_eq!(
            typed.node(chain[0]).output_row().unwrap().names(),
            vec!["h_id_v2"]
        );
    }

    #[test]
    fn overloaded_helper_uses_the_checker_selected_definition() {
        let src = "ints : query { id = int } = table \"s\" \"ints\"\n\
             strings : query { name = string } = table \"s\" \"strings\"\n\
             helper : query { id = int } -> query { h_id = int } = q => q & prefix \"h_\"\n\
             helper : query { name = string } -> query { name_v2 = string } = q => q & suffix \"_v2\"\n\
             q1 = ints & helper\n\
             q2 = strings & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let input = CompilerInput::new(&ws, &tc);
        let helper_defs: Vec<_> = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .enumerate()
            .filter_map(|(index, def)| (def.name == "helper").then_some(index))
            .collect();
        for (name, expected, helper_def) in [
            ("q1", "h_id", helper_defs[0]),
            ("q2", "name_v2", helper_defs[1]),
        ] {
            let def = ws.modules[ws.root]
                .module
                .defs
                .iter()
                .position(|d| d.name == name)
                .unwrap();
            let body = &ws.modules[ws.root].module.defs[def].body;
            let (typed, checked) = lower(input, ws.root, def, body).unwrap().unwrap();
            let terms =
                crate::normalize::elaborate_query_via_terms(input, ws.root, def, body).unwrap();
            compare_with_terms(input, &typed, &checked, &terms).unwrap();
            assert_eq!(checked.row().names(), &[expected]);
            let stage = typed
                .nodes()
                .find(|node| node.kind() != QueryNodeKind::Table)
                .unwrap();
            assert_eq!(stage.site().definition, helper_def);
        }
    }

    #[test]
    fn helper_can_follow_a_typed_query_alias() {
        let src = "users : query { id = int } = table \"s\" \"users\"\n\
             renamed = users & prefix \"u_\"\n\
             helper : query { u_id = int } -> query { h_u_id = int } = q => q & prefix \"h_\"\n\
             q = renamed & helper\n";
        let ws = Workspace::from_source(src);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let (typed, checked) = lower(
            CompilerInput::new(&ws, &tc),
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        assert_eq!(typed.chain().len(), 3);
        assert_eq!(checked.row().names(), &["h_u_id"]);
    }

    #[test]
    fn imported_helper_keeps_its_definition_identity() {
        let root_path = std::path::PathBuf::from("/tmp/cagara-typed-helper/main.cagara");
        let lib_path = std::path::PathBuf::from("/tmp/cagara-typed-helper/lib.cagara");
        let root = "import \"lib.cagara\" as lib\n\
                    users : query { id = int } = table \"s\" \"users\"\n\
                    q = users & lib.helper\n";
        let lib = "helper : query { id = int } -> query { h_id = int } = q => q & prefix \"h_\"\n";
        let buffers = std::collections::HashMap::from([(lib_path, lib.to_string())]);
        let ws = Workspace::open_with_buffers(&root_path, root.into(), &buffers);
        let tc = check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .unwrap();
        let (typed, checked) = lower(
            CompilerInput::new(&ws, &tc),
            ws.root,
            def,
            &ws.modules[ws.root].module.defs[def].body,
        )
        .unwrap()
        .unwrap();
        let helper_module = ws
            .module_for_path(&std::path::PathBuf::from(
                "/tmp/cagara-typed-helper/lib.cagara",
            ))
            .unwrap();
        assert_eq!(typed.root_kind(), QueryNodeKind::Prefix);
        assert_eq!(checked.row().names(), &["h_id"]);
        assert_eq!(
            typed.node(typed.root()).site().definition_module,
            helper_module
        );
    }
}
