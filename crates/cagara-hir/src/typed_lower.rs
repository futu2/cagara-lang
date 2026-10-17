//! Tree-driven query lowering slices.
//!
//! This builder consumes the checker-owned stage records and the typed query
//! nodes' rows. It handles direct pipelines and recursively composed joins and
//! set operations over tables or already typed query aliases. Some expression
//! payloads remain on the normalized bridge, and shapes whose stage bodies
//! cannot yet be instantiated return `None` for the established term route.
#![allow(dead_code)] // Keep the slice self-contained while later stages land.

use crate::check::{CombineKind, NodeId, NodeRef, QueryNodeKind, Row, RowStage, TypedQuery};
use crate::checked::CheckedQuery;
use crate::compile::CompilerInput;
use crate::core::{Error, Origin, RowType};
use crate::normalize::Resolved;
use cagara_syntax::ast::{Expr, ExprKind, Lit};
use std::collections::{HashMap, HashSet};

/// Build the typed query and its checked counterpart when this slice applies.
pub(crate) fn lower(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let Some((mut typed, checked)) = lower_unhydrated(input, module, def, body)? else {
        return Ok(None);
    };
    typed
        .carry_checked_payloads(&checked)
        .map_err(query_error)?;
    typed.require_checked_payloads().map_err(query_error)?;
    let rebuilt = typed.rebuild_checked(&checked).map_err(query_error)?;
    Ok(Some((typed, rebuilt.unwrap_or(checked))))
}

fn lower_unhydrated(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();

    // A standalone join/set can be rebuilt from its typed operands before
    // the composed pipeline route uses the normalized template.
    if let Some(lowered) = lower_combine_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }

    // A combine can occur below or above ordinary stages. Build the complete
    // relational skeleton recursively so a stage never causes the combine
    // child to be dropped. Expression payloads still come from the normalized
    // route; this builder only owns node identity and row boundaries.
    if contains_combine_tree(input, module, body) {
        if let Some(lowered) = lower_composed_pipeline(input, module, def, body)? {
            return Ok(Some(lowered));
        }
    }

    if let Some(lowered) = lower_keep_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_projection_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_where_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_simple_order_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    if let Some(lowered) = lower_plain_expression_chain(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    // The expression-bearing stages use the checked payload bridge for their
    // predicates, fields, and sort keys. Their relational boundaries still
    // come from the typed stage records, so the query arena does not recover
    // rows from the composed result type.
    if let Some(lowered) = lower_expression_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }

    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    // A name or qualified projection can be an alias of a query definition.
    // Instantiate the already-typed target tree at this use while preserving
    // the target's stable stage identities. Plain table aliases fall through:
    // they have no stages and are handled by `table_parts` below.
    if stages.is_empty() {
        if let Some(lowered) = lower_helper_pipeline(input, module, def, body)? {
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
                if let Some(lowered) = lower(input, target_module, target_def, target_body)? {
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
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.len() != 1
        || !matches!(stages[0].kind, RowStage::Select | RowStage::Update)
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
    let (child, mut checked, mut typed) = if let Some((schema, table, columns)) =
        table_parts(input, module, base)
    {
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
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
        _ => unreachable!(),
    };
    let id = typed
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
        _ => unreachable!(),
    }
    .map_err(|error| Error::unsupported(error.message).at(stage_origin))?;
    Ok(Some((typed.finish(id), checked)))
}

/// Lower a direct filter whose predicate uses only fields, literals, and the
/// scalar comparison/boolean operators declared in the prelude.
fn lower_simple_where_pipeline(
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
    let (child, checked, mut typed) = if let Some((schema, table, columns)) =
        table_parts(input, module, base)
    {
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
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

/// Build the plain window specification subset directly from source. Named
/// specs are followed through their resolved definition; frames remain on the
/// normalized bridge until their bounds can be built from checker facts.
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
            // The frame grammar has source-level constructors whose primitive
            // identities are not represented in the typed expression tree yet.
            crate::rules::winspec::FRAME => return Ok(None),
            _ => return Ok(None),
        }
    }
    crate::checked::window_spec(partition, order, None)
        .map(Some)
        .map_err(|error| Error::unsupported(error.message).at(Origin::new(module, expr.span)))
}

/// Lower a direct order whose keys fit the source checked-expression subset.
fn lower_simple_order_pipeline(
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
    let (child, checked, mut typed) = if let Some((schema, table, columns)) =
        table_parts(input, module, base)
    {
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
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
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.len() < 2
        || contains_distinct_stage(input, module, body)
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
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let mut syntax = HashMap::new();
    index_exprs(body, &mut syntax);
    let (mut child, mut checked, mut typed) = if let Some((schema, table, columns)) =
        table_parts(input, module, base)
    {
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
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
                    return Ok(None);
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
    let Some((left_expr, operation)) = find_combine_context(body, combine.site) else {
        return Ok(None);
    };
    let ExprKind::App(_, args) = &operation.kind else {
        return Ok(None);
    };
    let Some(right_expr) = args.first() else {
        return Ok(None);
    };
    let mut typed = TypedQuery::new();
    let Some((left, left_checked)) =
        lower_combine_input(input, module, def, left_expr, &mut typed)?
    else {
        return Ok(None);
    };
    let Some((right, right_checked)) =
        lower_combine_input(input, module, def, right_expr, &mut typed)?
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
        CombineKind::Set(_) => (right, left, right_checked, left_checked),
    };
    let output_row = combine.output.clone().map(Row::new);
    let origin = Origin::new(combine.node.definition_module, operation.span);
    let checked = match combine.kind {
        CombineKind::Set(kind) => CheckedQuery::set(kind, checked_left, checked_right, origin)
            .map_err(|error| Error::unsupported(error.message).at(origin))?,
        CombineKind::Join(kind) => {
            let predicate = plain_join_predicate(input, module, operation).or_else(|| {
                crate::normalize::elaborate_query_via_terms(input, module, def, body)
                    .ok()
                    .and_then(|terms| checked_join_predicate(&terms, terms.origin()))
            });
            let Some(predicate) = predicate else {
                return Ok(None);
            };
            CheckedQuery::join(kind, checked_left, checked_right, predicate, origin)
                .map_err(|error| Error::unsupported(error.message).at(origin))?
        }
    };
    let root = typed
        .combine(
            combine.node,
            kind,
            typed_left,
            typed_right,
            output_row.clone(),
            origin,
            checked_join_predicate(&checked, origin),
        )
        .map_err(query_error)?;
    let Some(output_row) = output_row else {
        return Ok(None);
    };
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
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    // A simple combine has no surrounding stages to compose. Let the direct
    // combine slice construct sets from its typed children, so their checked
    // relation does not need the normalized bridge.
    let direct_set = match &body.kind {
        ExprKind::App(head, args) if args.len() == 2 && is_pipe_head(head) => input
            .type_check()
            .typed_node(module, args[1].id)
            .and_then(|node| node.combine.as_ref())
            .is_some_and(|combine| matches!(combine.kind, CombineKind::Set(_))),
        _ => false,
    };
    if direct_set {
        return Ok(None);
    }
    let mut typed = TypedQuery::new();
    if let Some((root, checked)) = lower_plain_composed_expr(input, module, def, body, &mut typed)?
    {
        return Ok(Some((typed.finish(root), checked)));
    }
    let mut typed = TypedQuery::new();
    let Some(root) = lower_composed_expr(input, module, def, body, &mut typed)? else {
        return Ok(None);
    };
    let checked = crate::normalize::elaborate_query_via_terms(input, module, def, body)?;
    Ok(Some((typed.finish(root), checked)))
}

/// Source-build a composed query when all joins and stages have plain
/// expression payloads. The checked relation travels with each typed node, so
/// a join and its surrounding stages need no normalized query template.
fn lower_plain_composed_expr(
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
        if (target_module, target_def) == (module, def) {
            return Ok(None);
        }
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let id = copy_typed_graph(&target_typed, typed)?;
        return Ok(Some((id, checked)));
    }
    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    if args.len() != 2 || !is_pipe_head(head) {
        return Ok(None);
    }
    let left_expr = &args[0];
    let rhs = &args[1];
    if let Some((combine, operation)) =
        combine_record_in(input.type_check(), module, rhs).filter(|(combine, _)| {
            combine.node.definition_module == module && combine.node.definition == def
        })
    {
        let Some((left, checked_left)) =
            lower_plain_composed_expr(input, module, def, left_expr, typed)?
        else {
            return Ok(None);
        };
        let mut operands = query_operand_args(input, module, operation);
        let Some(right_expr) = operands.next() else {
            return Ok(None);
        };
        let Some((right, checked_right)) =
            lower_plain_composed_expr(input, module, def, right_expr, typed)?
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
                        output,
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

    let Some(stage) = stage_record(input.type_check(), module, def, rhs.id)
        .filter(|stage| stage.node.definition_module == module && stage.node.definition == def)
    else {
        return Ok(None);
    };
    let Some((child, checked)) = lower_plain_composed_expr(input, module, def, left_expr, typed)?
    else {
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
                return Ok(None);
            }
        }
        RowStage::Distinct => return Ok(None),
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
        RowStage::Distinct => return Ok(None),
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
        let _ = (schema, table); // table metadata belongs to the term bridge.
        return Ok(Some(id));
    }

    if let Some((target_module, target_def)) = query_target(input.type_check(), module, expr) {
        if (target_module, target_def) == (module, def) {
            return Ok(None);
        }
        let target_body = &input.module(target_module).def(target_def).body;
        let Some((target_typed, _)) = lower(input, target_module, target_def, target_body)? else {
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
        let rhs = &args[1];
        let has_combine = combine_record_in(input.type_check(), module, rhs).is_some();
        let has_stage = stage_record(input.type_check(), module, def, rhs.id).is_some();
        if !has_combine && !has_stage {
            if let Some(id) = lower_composed_helper(input, module, def, expr, typed)? {
                return Ok(Some(id));
            }
        }
        let Some(left) = lower_composed_expr(input, module, def, &args[0], typed)? else {
            return Ok(None);
        };
        if let Some((combine, operation)) = combine_record_in(input.type_check(), module, rhs) {
            let Some(right_expr) = query_operand_args(input, module, operation).next() else {
                return Ok(None);
            };
            let Some(right) = lower_composed_expr(input, module, def, right_expr, typed)? else {
                return Ok(None);
            };
            let kind = combine_query_kind(combine.kind);
            let output = combine.output.clone().map(Row::new);
            let origin = Origin::new(combine.node.definition_module, operation.span);
            let checked = crate::normalize::elaborate_query_via_terms(
                input,
                module,
                def,
                &input.module(module).def(def).body,
            )?;
            let join_predicate = checked_join_predicate(&checked, origin);
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
    // same combine record is used; predicates are simply ignored here because
    // their checked expression remains on the normalized bridge.
    if let Some((combine, operation)) = combine_record_in(input.type_check(), module, expr) {
        let operands: Vec<_> = query_operand_args(input, module, operation).collect();
        if operands.len() != 2 {
            return Ok(None);
        }
        let Some(left) = lower_composed_expr(input, module, def, operands[0], typed)? else {
            return Ok(None);
        };
        let Some(right) = lower_composed_expr(input, module, def, operands[1], typed)? else {
            return Ok(None);
        };
        let kind = combine_query_kind(combine.kind);
        let output = combine.output.clone().map(Row::new);
        let origin = Origin::new(combine.node.definition_module, operation.span);
        let checked = crate::normalize::elaborate_query_via_terms(
            input,
            module,
            def,
            &input.module(module).def(def).body,
        )?;
        let join_predicate = checked_join_predicate(&checked, origin);
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

    if let Some(id) = lower_composed_helper(input, module, def, expr, typed)? {
        return Ok(Some(id));
    }
    Ok(None)
}

fn lower_composed_helper(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    target: &mut TypedQuery,
) -> Result<Option<NodeId>, Error> {
    let ExprKind::App(head, args) = &expr.kind else {
        return Ok(None);
    };
    if args.len() != 2 || !is_pipe_head(head) {
        return Ok(None);
    }
    let tc = input.type_check();
    let Some(((helper_module, helper_def), _helper_args)) =
        helper_application(tc, module, &args[1])
    else {
        return Ok(None);
    };
    if !matches!(
        input.module(helper_module).def(helper_def).body.kind,
        ExprKind::Lambda(_, _)
    ) {
        return Ok(None);
    }
    let helper_stages: Vec<_> = tc.row_stages(helper_module, helper_def).cloned().collect();
    if helper_stages.is_empty() {
        return Ok(None);
    }
    let projections = helper_stages
        .iter()
        .filter(|stage| {
            matches!(
                stage.kind,
                RowStage::Select | RowStage::Update | RowStage::Agg
            )
        })
        .count();
    if helper_stages.iter().any(|stage| {
        !matches!(
            stage.kind,
            RowStage::Where
                | RowStage::Order
                | RowStage::Limit
                | RowStage::Offset
                | RowStage::Distinct
                | RowStage::Select
                | RowStage::Update
                | RowStage::Agg
        )
    }) {
        let mut source = TypedQuery::new();
        let Some((root, _)) = lower_helper_expr(input, module, def, expr, &mut source)? else {
            return Ok(None);
        };
        return copy_typed_graph(&source.finish(root), target).map(Some);
    }
    let Some(child) = lower_composed_expr(input, module, def, &args[0], target)? else {
        return Ok(None);
    };
    let mut syntax = HashMap::new();
    index_exprs(
        &input.module(helper_module).def(helper_def).body,
        &mut syntax,
    );
    let mut child = child;
    let helper_output = tc
        .typed_node(module, expr.id)
        .and_then(|node| node.row())
        .map(Row::new);
    let instantiated_rows = if projections > 0 {
        helper_stage_rows(input, module, def, expr, &helper_stages)
    } else {
        None
    };
    if projections > 0 && helper_output.is_none() && instantiated_rows.is_none() {
        return Ok(None);
    }
    if projections > 1 && instantiated_rows.is_none() {
        return Ok(None);
    }
    for (index, stage) in helper_stages.into_iter().enumerate() {
        let origin = Origin::new(
            helper_module,
            syntax.get(&stage.site).map_or(stage.span, |expr| expr.span),
        );
        let projected_row = instantiated_rows
            .as_ref()
            .and_then(|rows| rows.get(index))
            .cloned()
            .or_else(|| helper_output.clone());
        let id = match stage.kind {
            RowStage::Select | RowStage::Update | RowStage::Agg => target
                .project(
                    stage.node,
                    match stage.kind {
                        RowStage::Select => QueryNodeKind::Select,
                        RowStage::Update => QueryNodeKind::Update,
                        RowStage::Agg => QueryNodeKind::Agg,
                        _ => unreachable!(),
                    },
                    child,
                    projected_row,
                    origin,
                    None,
                )
                .map_err(query_error)?,
            _ => typed_keep_stage(target, child, stage.kind, stage.node, origin)?,
        };
        let node = target.node(id);
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
        child = id;
    }
    Ok(Some(child))
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let id = copy_typed_graph(&target_typed, typed)?;
        return Ok(Some((id, checked)));
    }
    Ok(None)
}

fn table_origin(input: CompilerInput<'_>, module: usize, expr: &Expr) -> Origin {
    match &expr.kind {
        ExprKind::App(_, _) => Origin::new(module, expr.span),
        ExprKind::Name(_) | ExprKind::Proj(_, _) => {
            if let Some(Resolved::Def((target_module, target_def))) =
                input.type_check().resolution(module, expr.id)
            {
                let target = &input.module(*target_module).def(*target_def).body;
                return table_origin(input, *target_module, target);
            }
            Origin::new(module, expr.span)
        }
        _ => Origin::new(module, expr.span),
    }
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

/// Lower a direct pipeline whose stages carry expressions (`where`, `select`,
/// `update`, `agg`, and `order`), together with any row-preserving stages mixed
/// into it. Expression payloads are hydrated from the normalized comparison
/// tree for now, while the relational structure and row boundaries are rebuilt
/// from `TypedQuery` before the normalized relation is compared.
fn lower_expression_pipeline(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let tc = input.type_check();
    let stages: Vec<_> = tc.row_stages(module, def).cloned().collect();
    if stages.is_empty()
        || stages.iter().all(|stage| {
            matches!(
                stage.kind,
                RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
            )
        })
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

    // A direct pipeline has one query leaf. This check is intentionally
    // structural: `find_base` alone would also find the left table inside a
    // join and could silently drop the right input.
    let stage_sites: HashSet<u32> = stages.iter().map(|stage| stage.site).collect();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let mut typed = TypedQuery::new();
    let (mut child, mut checked) = if let Some((schema, table, columns)) =
        table_parts(input, module, base)
    {
        let base_origin = table_origin(input, module, base);
        let checked = CheckedQuery::table(
            schema,
            table,
            Some(RowType::new(columns.clone())),
            base_origin,
        )
        .map_err(|error| Error::unsupported(error.message).at(base_origin))?;
        let child = typed
            .table(NodeRef::new(module, def, base.id), base_origin, columns)
            .map_err(query_error)?;
        (child, checked)
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
        else {
            return Ok(None);
        };
        let child = copy_typed_graph(&target_typed, &mut typed)?;
        (child, checked)
    } else {
        return Ok(None);
    };
    let terms = crate::normalize::elaborate_query_via_terms(input, module, def, body)?;
    let base_node_count = checked.node_chain().len();
    let mut syntax = HashMap::new();
    index_exprs(body, &mut syntax);
    let mut where_payloads = terms
        .node_chain_predicates()
        .into_iter()
        .skip(base_node_count)
        .filter(|(kind, _)| *kind == crate::checked::QueryKind::Where)
        .map(|(_, predicate)| predicate);
    let mut order_payloads = terms
        .node_chain_order_keys()
        .into_iter()
        .skip(base_node_count)
        .filter(|(kind, _)| *kind == crate::checked::QueryKind::Order)
        .map(|(_, keys)| keys);
    let mut projection_payloads = terms
        .node_chain_projection_fields()
        .into_iter()
        .skip(base_node_count)
        .filter(|(kind, _)| {
            matches!(
                kind,
                crate::checked::QueryKind::Select
                    | crate::checked::QueryKind::Update
                    | crate::checked::QueryKind::Agg
            )
        })
        .map(|(_, fields)| fields);
    let mut checked_origins = terms
        .node_chain()
        .into_iter()
        .skip(base_node_count)
        .filter(|(kind, _, _)| *kind != crate::checked::QueryKind::Table)
        .map(|(_, origin, _)| origin);

    for stage in stages {
        let input_row = Row::new(stage.input.clone().expect("checked above"));
        let output_row = Row::new(stage.output.clone().expect("checked above"));
        let origin = Origin::new(
            stage.node.definition_module,
            syntax.get(&stage.site).map_or(stage.span, |expr| expr.span),
        );
        let checked_origin = checked_origins.next().unwrap_or(origin);
        let predicate = if stage.kind == RowStage::Where {
            where_payloads.next().flatten()
        } else {
            None
        };
        let order_keys = if stage.kind == RowStage::Order {
            order_payloads.next().flatten()
        } else {
            None
        };
        let projection_fields = if matches!(
            stage.kind,
            RowStage::Select | RowStage::Update | RowStage::Agg
        ) {
            projection_payloads.next().flatten()
        } else {
            None
        };
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
                    Some(input_row.clone()),
                    Some(output_row.clone()),
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
                    Some(output_row.clone()),
                    origin,
                    projection_fields.clone(),
                )
                .map_err(query_error)?,
            RowStage::Where
            | RowStage::Order
            | RowStage::Limit
            | RowStage::Offset
            | RowStage::Distinct => typed
                .keep_row_with_payload(
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
                    predicate.clone(),
                    order_keys.clone(),
                    None,
                    None,
                )
                .map_err(query_error)?,
        };
        checked = match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
                let Some(label) = stage.label.clone() else {
                    return Ok(None);
                };
                match stage.kind {
                    RowStage::Prefix => CheckedQuery::prefix(checked, label, checked_origin),
                    RowStage::Suffix => CheckedQuery::suffix(checked, label, checked_origin),
                    RowStage::Omit => CheckedQuery::omit(checked, label, checked_origin),
                    RowStage::MapValue => CheckedQuery::map_value(checked, label, checked_origin),
                    _ => unreachable!(),
                }
            }
            RowStage::Where => {
                let Some(predicate) = predicate else {
                    return Ok(None);
                };
                CheckedQuery::where_(checked, predicate, checked_origin)
            }
            RowStage::Select => {
                let Some(fields) = projection_fields else {
                    return Ok(None);
                };
                CheckedQuery::select(checked, fields, checked_origin)
            }
            RowStage::Update => {
                let Some(fields) = projection_fields else {
                    return Ok(None);
                };
                CheckedQuery::update(checked, fields, checked_origin)
            }
            RowStage::Agg => {
                let Some(fields) = projection_fields else {
                    return Ok(None);
                };
                CheckedQuery::agg(checked, fields, checked_origin)
            }
            RowStage::Order => {
                let Some(keys) = order_keys else {
                    return Ok(None);
                };
                CheckedQuery::order(checked, keys, checked_origin)
            }
            RowStage::Limit => {
                let Some(count) = syntax.get(&stage.site).and_then(stage_count) else {
                    return Ok(None);
                };
                CheckedQuery::limit(checked, count, checked_origin)
            }
            RowStage::Offset => {
                let Some(count) = syntax.get(&stage.site).and_then(stage_count) else {
                    return Ok(None);
                };
                CheckedQuery::offset(checked, count, checked_origin)
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

    // Some row-preserving wrappers (notably `distinct`) are absorbed by the
    // checker stage records. Keep the normalized tree when its shape contains
    // an operation that the direct typed chain did not record.
    let checked = if checked
        .node_chain()
        .iter()
        .map(|(kind, _, _)| *kind)
        .eq(terms.node_chain().iter().map(|(kind, _, _)| *kind))
    {
        checked
    } else {
        terms
    };
    let typed_columns = typed
        .node(child)
        .output_row()
        .map(|row| row.columns().to_vec())
        .unwrap_or_default();
    if checked.row().columns() != typed_columns.as_slice() {
        return Err(Error::unsupported(
            "typed expression pipeline produced a row different from its checked query",
        )
        .at(checked.origin()));
    }
    // The relational structure is represented by the typed arena; the public
    // wrapper rebuilds a checked tree from that arena when its shape aligns
    // with the normalized comparison tree.
    Ok(Some((typed.finish(child), checked)))
}

/// Whether a query body is one direct `&` chain over a table (or a table alias)
/// and primitive stages. This excludes joins, sets, and helper applications,
/// whose query graph needs more than one child or a later instantiation slice.
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
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let mut typed = TypedQuery::new();
    let Some((root, checked)) = lower_keep_expr(input, module, def, body, &mut typed)? else {
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
                lower(input, target_module, target_def, target_body)?
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
    let Some((child, mut checked)) = lower_keep_expr(input, module, def, &args[0], typed)? else {
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
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Option<(TypedQuery, CheckedQuery)>, Error> {
    let mut typed = TypedQuery::new();
    let Some((root, checked)) = lower_helper_expr(input, module, def, body, &mut typed)? else {
        return Ok(None);
    };
    Ok(Some((typed.finish(root), checked)))
}

fn lower_helper_expr(
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
        let Some((target_typed, checked)) = lower(input, target_module, target_def, target_body)?
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
        if args.len() == 2 && is_pipe_head(head) {
            let Some((child, checked)) = lower_helper_expr(input, module, def, &args[0], typed)?
            else {
                return Ok(None);
            };
            let Some(((helper_module, helper_def), helper_args)) =
                helper_application(tc, module, &args[1])
            else {
                return Ok(None);
            };
            return apply_helper(
                input,
                module,
                def,
                body.id,
                child,
                checked,
                helper_module,
                helper_def,
                &helper_args,
                body,
                typed,
            );
        }
    }

    Ok(None)
}

fn apply_helper(
    input: CompilerInput<'_>,
    use_module: usize,
    use_def: usize,
    use_site: u32,
    mut child: NodeId,
    mut checked: CheckedQuery,
    helper_module: usize,
    helper_def: usize,
    helper_args: &[Expr],
    use_expr: &Expr,
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let tc = input.type_check();
    if !matches!(
        input.module(helper_module).def(helper_def).body.kind,
        ExprKind::Lambda(_, _)
    ) {
        return Ok(None);
    }
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
    let mut syntax = HashMap::new();
    index_exprs(helper_syntax, &mut syntax);
    let projections = helper_stages
        .iter()
        .filter(|stage| {
            matches!(
                stage.kind,
                RowStage::Select | RowStage::Update | RowStage::Agg
            )
        })
        .count();
    // The normalized relation remains the payload oracle while helper
    // expressions are instantiated at their use site. The typed nodes below
    // own those payloads; only their concrete intermediate rows need the
    // checked tree's already-instantiated values.
    let normalized = Some(crate::normalize::elaborate_query_via_terms(
        input, use_module, use_def, use_expr,
    )?);
    let instantiated_rows = helper_stage_rows_from_checked(
        input,
        use_module,
        use_def,
        use_expr,
        &helper_stages,
        normalized.as_ref().expect("normalized helper query"),
    );
    if projections > 1 && instantiated_rows.is_none() {
        return Ok(None);
    }
    let payloads = helper_stage_payloads(
        normalized.as_ref().expect("normalized helper query"),
        &helper_stages,
    );
    if payloads.is_none() {
        return Ok(None);
    }
    let payloads = payloads.expect("checked helper payloads");
    for (index, stage) in helper_stages.into_iter().enumerate() {
        let origin = Origin::new(
            helper_module,
            syntax.get(&stage.site).map_or(stage.span, |expr| expr.span),
        );
        let input_columns = checked.row().columns().to_vec();
        if stage
            .input
            .as_deref()
            .is_some_and(|expected| expected != input_columns)
        {
            return Ok(None);
        }
        let input_row = Row::new(input_columns);
        let projected_row = instantiated_rows
            .as_ref()
            .and_then(|rows| rows.get(index))
            .cloned();
        let payload = payloads[index].clone();
        let kind = match stage.kind {
            RowStage::Prefix => Some(QueryNodeKind::Prefix),
            RowStage::Suffix => Some(QueryNodeKind::Suffix),
            RowStage::Omit => Some(QueryNodeKind::Omit),
            RowStage::MapValue => Some(QueryNodeKind::MapValue),
            _ => None,
        };
        checked = match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => {
                let Some(label) = stage
                    .label
                    .clone()
                    .or_else(|| helper_witness(&stage, &syntax, &params, helper_args))
                else {
                    return Ok(None);
                };
                match stage.kind {
                    RowStage::Prefix => CheckedQuery::prefix(checked, label, origin),
                    RowStage::Suffix => CheckedQuery::suffix(checked, label, origin),
                    RowStage::Omit => CheckedQuery::omit(checked, label, origin),
                    RowStage::MapValue => CheckedQuery::map_value(checked, label, origin),
                    _ => unreachable!(),
                }
            }
            RowStage::Where => {
                let Some(predicate) = payload.predicate.clone() else {
                    return Ok(None);
                };
                CheckedQuery::where_(checked, predicate, origin)
            }
            RowStage::Order => {
                let Some(keys) = payload.order_keys.clone() else {
                    return Ok(None);
                };
                CheckedQuery::order(checked, keys, origin)
            }
            RowStage::Limit => {
                let Some(count) = syntax.get(&stage.site).and_then(stage_count) else {
                    return Ok(None);
                };
                CheckedQuery::limit(checked, count, origin)
            }
            RowStage::Offset => {
                let Some(count) = syntax.get(&stage.site).and_then(stage_count) else {
                    return Ok(None);
                };
                CheckedQuery::offset(checked, count, origin)
            }
            RowStage::Distinct => CheckedQuery::distinct(checked, origin),
            RowStage::Select => {
                let Some(fields) = payload.projection_fields.clone() else {
                    return Ok(None);
                };
                CheckedQuery::select(checked, fields, origin)
            }
            RowStage::Update => {
                let Some(fields) = payload.projection_fields.clone() else {
                    return Ok(None);
                };
                CheckedQuery::update(checked, fields, origin)
            }
            RowStage::Agg => {
                let Some(fields) = payload.projection_fields.clone() else {
                    return Ok(None);
                };
                CheckedQuery::agg(checked, fields, origin)
            }
        }
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let output_columns = projected_row
            .as_ref()
            .map(|row| row.columns().to_vec())
            .unwrap_or_else(|| checked.row().columns().to_vec());
        if stage
            .output
            .as_deref()
            .is_some_and(|expected| expected != output_columns.as_slice())
        {
            return Ok(None);
        }
        let output_row = Row::new(output_columns);
        let id = match stage.kind {
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue => typed
                .rewrite(
                    stage.node,
                    kind.expect("rewrite stage has a rewrite kind"),
                    child,
                    Some(input_row),
                    Some(output_row.clone()),
                    origin,
                )
                .map_err(query_error)?,
            RowStage::Where | RowStage::Order => typed
                .keep_row_with_payload(
                    stage.node,
                    match stage.kind {
                        RowStage::Where => QueryNodeKind::Where,
                        RowStage::Order => QueryNodeKind::Order,
                        _ => unreachable!(),
                    },
                    child,
                    origin,
                    payload.predicate.clone(),
                    payload.order_keys.clone(),
                    None,
                    None,
                )
                .map_err(query_error)?,
            RowStage::Limit | RowStage::Offset | RowStage::Distinct => {
                typed_keep_stage(typed, child, stage.kind, stage.node, origin)?
            }
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
                    projected_row,
                    origin,
                    payload.projection_fields.clone(),
                )
                .map_err(query_error)?,
        };
        child = id;
    }
    let result_row = tc
        .typed_node(use_module, use_site)
        .and_then(|node| node.row())
        .or_else(|| {
            normalized
                .as_ref()
                .map(|query| query.row().columns().to_vec())
        })
        .unwrap_or_else(|| checked.row().columns().to_vec());
    if tc
        .typed_node(use_module, use_site)
        .and_then(|node| node.row())
        .is_some_and(|expected| expected != result_row.as_slice())
    {
        return Ok(None);
    }
    // The normalized wrapper retains the call-site origins introduced by
    // helper inlining. Keep it as the public checked template while the typed
    // graph above remains the source of stage structure and payload ownership.
    Ok(Some((child, normalized.expect("normalized helper query"))))
}

/// Instantiate a helper when every stage payload is in the plain checked
/// expression subset. The helper's source nodes keep their identities while
/// the caller's concrete row is threaded through the whole stage chain.
///
/// This is deliberately transactional: a failed plain lowering must leave the
/// caller's typed arena untouched so the normalized fallback can build the
/// same child graph from the original `child` id.
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
    if stages.len() > 1
        && stages
            .iter()
            .any(|stage| matches!(stage.kind, RowStage::Select | RowStage::Update))
    {
        return Ok(None);
    }
    let helper_body = &input.module(helper_module).def(helper_def).body;
    // Helper expressions are checked once at their generic definition. Window
    // templates need the instantiated call-site type, so keep them on the
    // checked payload bridge until helper expression instantiation is typed.
    let mut helper_exprs = HashMap::new();
    index_exprs(helper_body, &mut helper_exprs);
    if helper_exprs.values().any(|expr| {
        matches!(&expr.kind, ExprKind::App(_, _) if declared_template(input, helper_module, expr)
            .is_some_and(|(_, kind, _)| kind == crate::normalize::TemplateKind::Win))
    }) {
        return Ok(None);
    }
    let mut syntax = HashMap::new();
    index_exprs(helper_body, &mut syntax);
    let params = lambda_params(helper_body);

    // Build on a clone so any unsupported expression can cleanly fall back to
    // the normalized route without retaining partially appended nodes.
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
    if !unordered_rows_match(expected.as_deref(), Some(checked.row().columns())) {
        return Ok(None);
    }
    *typed = candidate;
    Ok(Some((current, checked)))
}

#[derive(Clone)]
struct HelperStagePayload {
    predicate: Option<crate::checked::CheckedExpr>,
    order_keys: Option<Vec<(crate::checked::CheckedExpr, bool)>>,
    projection_fields: Option<Vec<(String, crate::checked::CheckedExpr)>>,
}

/// Align expression payloads from the instantiated checked helper query with
/// the helper's stage records. The helper stages are the tail of the unary
/// query chain; any stages on the caller's child precede them.
fn helper_stage_payloads(
    checked: &CheckedQuery,
    stages: &[crate::check::RowStageView],
) -> Option<Vec<HelperStagePayload>> {
    let chain = checked.node_chain();
    if stages.len() > chain.len() {
        return None;
    }
    let start = chain.len() - stages.len();
    let predicates = checked.node_chain_predicates();
    let order_keys = checked.node_chain_order_keys();
    let projection_fields = checked.node_chain_projection_fields();
    stages
        .iter()
        .enumerate()
        .map(|(index, stage)| {
            let at = start + index;
            if !checked_kind_matches(chain[at].0, stage.kind) {
                return None;
            }
            Some(HelperStagePayload {
                predicate: predicates.get(at).and_then(|(_, value)| value.clone()),
                order_keys: order_keys.get(at).and_then(|(_, value)| value.clone()),
                projection_fields: projection_fields
                    .get(at)
                    .and_then(|(_, value)| value.clone()),
            })
        })
        .collect()
}

fn helper_stage_rows_from_checked(
    input: CompilerInput<'_>,
    module: usize,
    _def: usize,
    expr: &Expr,
    stages: &[crate::check::RowStageView],
    checked: &CheckedQuery,
) -> Option<Vec<Row>> {
    let chain = checked.node_chain();
    if stages.len() > chain.len() {
        return None;
    }
    let start = chain.len() - stages.len();
    if !chain[start..]
        .iter()
        .zip(stages)
        .all(|((kind, _, _), stage)| checked_kind_matches(*kind, stage.kind))
    {
        return None;
    }
    let final_row = input
        .type_check()
        .typed_node(module, expr.id)
        .and_then(|node| node.row());
    Some(
        chain[start..]
            .iter()
            .map(|(_, _, row)| {
                let columns = row
                    .columns()
                    .iter()
                    .map(|(name, ty)| {
                        if matches!(ty, crate::core::ScalarType::Unknown) {
                            final_row
                                .as_ref()
                                .and_then(|fields| fields.iter().find(|(n, _)| n == name))
                                .map_or((name.clone(), ty.clone()), |(_, ty)| {
                                    (name.clone(), ty.clone())
                                })
                        } else {
                            (name.clone(), ty.clone())
                        }
                    })
                    .collect();
                Row::new(columns)
            })
            .collect(),
    )
}

/// Read the instantiated output row of each helper stage from the checked
/// query for this use.  Helper definitions intentionally keep their stage
/// rows open; the use expression is where the callee's scheme is instantiated
/// and the checked bridge has the concrete intermediate rows.
fn helper_stage_rows(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    expr: &Expr,
    stages: &[crate::check::RowStageView],
) -> Option<Vec<Row>> {
    let checked = crate::normalize::elaborate_query_via_terms(input, module, def, expr).ok()?;
    let chain = checked.node_chain();
    if stages.len() > chain.len() {
        return None;
    }
    let start = chain.len() - stages.len();
    let rows = chain[start..]
        .iter()
        .zip(stages)
        .all(|((kind, _, _), stage)| checked_kind_matches(*kind, stage.kind));
    if !rows {
        return None;
    }
    let final_row = input
        .type_check()
        .typed_node(module, expr.id)
        .and_then(|node| node.row());
    Some(
        chain[start..]
            .iter()
            .map(|(_, _, row)| {
                let columns = row
                    .columns()
                    .iter()
                    .map(|(name, ty)| {
                        if matches!(ty, crate::core::ScalarType::Unknown) {
                            final_row
                                .as_ref()
                                .and_then(|fields| fields.iter().find(|(n, _)| n == name))
                                .map_or((name.clone(), ty.clone()), |(_, ty)| {
                                    (name.clone(), ty.clone())
                                })
                        } else {
                            (name.clone(), ty.clone())
                        }
                    })
                    .collect();
                Row::new(columns)
            })
            .collect(),
    )
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

/// Check the first typed slice against the normalized term route before its
/// result is allowed onto the production path.
pub(crate) fn compare_with_terms(
    input: CompilerInput<'_>,
    typed: &TypedQuery,
    checked: &CheckedQuery,
    terms: &CheckedQuery,
) -> Result<(), Error> {
    let fail = |message: &str| Error::unsupported(message).at(checked.origin());
    if checked.row() != terms.row() {
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

fn lambda_params(expr: &Expr) -> Vec<String> {
    let mut params = Vec::new();
    let mut body = expr;
    while let ExprKind::Lambda(parameter, next) = &body.kind {
        params.push(parameter.clone());
        body = next;
    }
    params
}

fn is_pipe_head(head: &Expr) -> bool {
    matches!(&head.kind, ExprKind::Name(name) if name == "_&_")
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
            let target = input.module(target_module).def(target_def);
            let (schema, table, _) = table_parts(input, target_module, &target.body)?;
            let columns = tc.scheme_fields(target_module, target_def)?;
            Some((schema, table, columns))
        }
        _ => None,
    }
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
    fn direct_expression_stages_build_typed_nodes() {
        let (typed, checked) = lower_source(
            "users : query { id = int, name = string, age = int } = table \"s\" \"users\"\n\
             q = users & where (.age > 1) & update { older = .age } & select { .name, .older } & order [asc .name]\n",
            "q",
        );
        let kinds: Vec<_> = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id).kind())
            .collect();
        assert_eq!(
            kinds,
            vec![
                QueryNodeKind::Order,
                QueryNodeKind::Select,
                QueryNodeKind::Update,
                QueryNodeKind::Where,
                QueryNodeKind::Table,
            ]
        );
        assert_eq!(checked.row().names(), &["name", "older"]);
        let where_node = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| node.kind() == QueryNodeKind::Where)
            .expect("typed pipeline has a where node");
        assert_eq!(
            where_node.predicate().map(|predicate| predicate.kind()),
            Some(crate::checked::ExprKind::Template)
        );
        let order_node = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| node.kind() == QueryNodeKind::Order)
            .expect("typed pipeline has an order node");
        let order_keys = order_node.order_keys().expect("order carries checked keys");
        assert_eq!(order_keys.len(), 1);
        assert_eq!(order_keys[0].0.kind(), crate::checked::ExprKind::Column);
        assert!(order_keys[0].1, "the source key is ascending");
        let update_node = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| node.kind() == QueryNodeKind::Update)
            .expect("typed pipeline has an update node");
        let update_fields = update_node
            .projection_fields()
            .expect("update carries checked fields");
        assert_eq!(update_fields.len(), 1);
        assert_eq!(update_fields[0].0, "older");
        assert_eq!(update_fields[0].1.kind(), crate::checked::ExprKind::Column);
        let select_node = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| node.kind() == QueryNodeKind::Select)
            .expect("typed pipeline has a select node");
        assert_eq!(
            select_node
                .projection_fields()
                .expect("select carries checked fields")
                .len(),
            2
        );

        let (typed, checked) = lower_source(
            "users : query { id = int, age = int } = table \"s\" \"users\"\n\
             q = users & agg { age = group .age, n = count }\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Agg);
        assert_eq!(checked.row().names(), &["age", "n"]);
        let agg_node = typed.node(typed.root());
        let agg_fields = agg_node
            .projection_fields()
            .expect("aggregate carries checked fields");
        assert_eq!(agg_fields.len(), 2);
        assert_eq!(agg_fields[0].0, "age");
        assert_eq!(agg_fields[1].0, "n");

        let (typed, checked) = lower_source(
            "spec = { order = [asc .id] }\n\
             users : query { id = int } = table \"s\" \"users\"\n\
             q = users & select { id = .id, rn = rowNumber spec }\n",
            "q",
        );
        assert_eq!(checked.row().names(), &["id", "rn"]);
        let window_node = typed.node(typed.root());
        let window_fields = window_node
            .projection_fields()
            .expect("select carries checked fields");
        assert_eq!(window_fields[1].0, "rn");
        assert_eq!(
            window_fields[1].1.kind(),
            crate::checked::ExprKind::WinTemplate
        );
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
    fn combines_can_be_nested_with_stages_and_typed_children() {
        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { id = int, uid = int, amount = float } = table \"s\" \"orders\"\n\
             q = users & innerJoin orders (.<id == .>uid) & where (.amount > 1.0)\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Where);
        let join = typed
            .chain()
            .into_iter()
            .map(|id| typed.node(id))
            .find(|node| matches!(node.kind(), QueryNodeKind::Join(_)))
            .expect("nested join");
        assert_eq!(join.inputs().len(), 2);
        assert_eq!(checked.row().names(), &["id", "name", "uid", "amount"]);

        let (typed, checked) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             vips : query { id = int, name = string } = table \"s\" \"vips\"\n\
             q = users & union vips & order [asc .name] & limit 3\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Limit);
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Set(_))));
        assert_eq!(checked.row().names(), &["id", "name"]);

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { id = int, uid = int } = table \"s\" \"orders\"\n\
             q = users & prefix \"u_\" & innerJoin orders (.<u_id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Prefix)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { id = int, uid = int, amount = float } = table \"s\" \"orders\"\n\
             q = users & innerJoin (orders & where (.amount > 1.0)) (.<id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Where)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { id = int, uid = int } = table \"s\" \"orders\"\n\
             joined = users & innerJoin orders (.<id == .>uid)\n\
             q = joined & limit 2\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Limit);
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { uid = int, amount = float } = table \"s\" \"orders\"\n\
             rename = affix => q => q & prefix affix\n\
             q = users & innerJoin (orders & rename \"o_\") (.<id == .>o_uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Prefix)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { uid = int, amount = float } = table \"s\" \"orders\"\n\
             filtered = q => q & where (.amount > 1.0)\n\
             q = users & innerJoin (orders & filtered) (.<id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Where)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { uid = int, amount = float } = table \"s\" \"orders\"\n\
             projected = q => q & select { .uid, .amount }\n\
             q = users & innerJoin (orders & projected) (.<id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Select)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { uid = int, amount = float } = table \"s\" \"orders\"\n\
             projected = q => q & where (.amount > 1.0) & select { .uid, .amount } & order [asc .uid]\n\
             q = users & innerJoin (orders & projected) (.<id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Where)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Select)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Order)));

        let (typed, _) = lower_source(
            "users : query { id = int, name = string } = table \"s\" \"users\"\n\
             orders : query { uid = int, amount = float } = table \"s\" \"orders\"\n\
             grouped = q => q & agg { uid = group .uid, n = count }\n\
             q = users & innerJoin (orders & grouped) (.<id == .>uid)\n",
            "q",
        );
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Agg)));
        assert!(typed
            .nodes()
            .any(|node| matches!(node.kind(), QueryNodeKind::Join(_))));
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
