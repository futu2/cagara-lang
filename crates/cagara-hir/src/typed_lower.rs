//! The first tree driven query lowering slice.
//!
//! This builder consumes the checker-owned stage records and the typed query
//! nodes' rows. It intentionally handles only a direct pipeline whose base is
//! a table and whose stages are `prefix`, `suffix`, `omit`, or `mapValue`.
//! Other shapes return `None` and remain on the established term route until a
//! later migration slice supplies their constructors and expression bridge.
#![allow(dead_code)] // Keep the slice self-contained while later stages land.

use crate::check::{NodeId, NodeRef, QueryNodeKind, Row, RowStage, TypedQuery};
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
    let tc = input.type_check();

    if let Some(lowered) = lower_keep_pipeline(input, module, def, body)? {
        return Ok(Some(lowered));
    }
    // The expression-bearing stages use the existing normalized expression
    // bridge for their predicates, fields, and sort keys. Their relational
    // boundaries still come from the typed stage records, so the query arena
    // does not recover rows from the composed result type.
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

/// Lower a direct pipeline whose stages carry expressions (`where`, `select`,
/// `update`, `agg`, and `order`), together with any row-preserving stages mixed
/// into it. The expression bridge remains the normalized term lowering used by
/// the established route; this slice moves the relational skeleton and its row
/// boundaries onto `TypedQuery`.
fn lower_expression_pipeline(
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

    // A direct pipeline has one query leaf. This check is intentionally
    // structural: `find_base` alone would also find the left table inside a
    // join and could silently drop the right input.
    let stage_sites: HashSet<u32> = stages.iter().map(|stage| stage.site).collect();
    let Some(base) = find_base(body, &stage_sites, tc, module) else {
        return Ok(None);
    };
    let Some((_, _, columns)) = table_parts(input, module, base) else {
        return Ok(None);
    };
    let terms = crate::normalize::elaborate_query_via_terms(input, module, def, body)?;
    let base_origin = Origin::new(module, base.span);
    let checked = terms;
    let mut typed = TypedQuery::new();
    let mut child = typed
        .table(NodeRef::new(module, def, base.id), base_origin, columns)
        .map_err(query_error)?;
    let mut syntax = HashMap::new();
    index_exprs(body, &mut syntax);

    for stage in stages {
        let input_row = Row::new(stage.input.clone().expect("checked above"));
        let output_row = Row::new(stage.output.clone().expect("checked above"));
        let origin = Origin::new(
            stage.node.definition_module,
            syntax.get(&stage.site).map_or(stage.span, |expr| expr.span),
        );
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
            return Err(Error::unsupported(format!(
                "typed stage {:?} produced rows different from its checked record",
                stage.kind
            ))
            .at(origin));
        }
        child = id;
    }

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
    // The checked value is intentionally the normalized expression bridge
    // result, while every relational node is represented by the typed arena.
    Ok(Some((typed.finish(child), checked)))
}

/// Whether a query body is one direct `&` chain over a table (or a table alias)
/// and primitive stages. This excludes joins, sets, and helper applications,
/// whose query graph needs more than one child or a later instantiation slice.
fn is_direct_pipeline(input: CompilerInput<'_>, module: usize, expr: &Expr) -> bool {
    if table_parts(input, module, expr).is_some() {
        return true;
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
        _ => return Ok(None),
    };
    let origin = Origin::new(module, args[1].span);
    let site = NodeRef::new(module, def, args[1].id);
    let id = typed
        .keep_row(site, kind, child, origin)
        .map_err(query_error)?;
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
                body.id,
                child,
                checked,
                helper_module,
                helper_def,
                &helper_args,
                typed,
            );
        }
    }

    Ok(None)
}

fn apply_helper(
    input: CompilerInput<'_>,
    use_module: usize,
    use_site: u32,
    mut child: NodeId,
    mut checked: CheckedQuery,
    helper_module: usize,
    helper_def: usize,
    helper_args: &[Expr],
    typed: &mut TypedQuery,
) -> Result<Option<(NodeId, CheckedQuery)>, Error> {
    let tc = input.type_check();
    if !matches!(
        input.module(helper_module).def(helper_def).body.kind,
        ExprKind::Lambda(_, _)
    ) {
        return Ok(None);
    }
    let helper_stages: Vec<_> = tc.row_stages(helper_module, helper_def).cloned().collect();
    if helper_stages.is_empty()
        || helper_stages.iter().any(|stage| {
            !matches!(
                stage.kind,
                RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
            )
        })
    {
        return Ok(None);
    }
    let helper_syntax = &input.module(helper_module).def(helper_def).body;
    let params = lambda_params(helper_syntax);
    if params.iter().collect::<HashSet<_>>().len() != params.len() {
        return Ok(None);
    }
    let mut syntax = HashMap::new();
    index_exprs(helper_syntax, &mut syntax);
    for stage in helper_stages {
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
        let kind = match stage.kind {
            RowStage::Prefix => QueryNodeKind::Prefix,
            RowStage::Suffix => QueryNodeKind::Suffix,
            RowStage::Omit => QueryNodeKind::Omit,
            RowStage::MapValue => QueryNodeKind::MapValue,
            _ => unreachable!(),
        };
        let Some(label) = stage
            .label
            .clone()
            .or_else(|| helper_witness(&stage, &syntax, &params, helper_args))
        else {
            return Ok(None);
        };
        checked = match stage.kind {
            RowStage::Prefix => CheckedQuery::prefix(checked, label, origin),
            RowStage::Suffix => CheckedQuery::suffix(checked, label, origin),
            RowStage::Omit => CheckedQuery::omit(checked, label, origin),
            RowStage::MapValue => CheckedQuery::map_value(checked, label, origin),
            _ => unreachable!(),
        }
        .map_err(|error| Error::unsupported(error.message).at(origin))?;
        let output_columns = checked.row().columns().to_vec();
        if stage
            .output
            .as_deref()
            .is_some_and(|expected| expected != output_columns.as_slice())
        {
            return Ok(None);
        }
        let output_row = Row::new(output_columns);
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
        child = id;
    }
    if tc
        .typed_node(use_module, use_site)
        .and_then(|node| node.row())
        .is_some_and(|expected| expected != checked.row().columns())
    {
        return Ok(None);
    }
    Ok(Some((child, checked)))
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
    let mut copied = HashMap::new();
    for old in source.chain().into_iter().rev() {
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
            _ => {
                let old_child = node
                    .inputs()
                    .first()
                    .copied()
                    .ok_or_else(|| Error::unsupported("typed stage has no child"))?;
                let child = *copied
                    .get(&old_child)
                    .ok_or_else(|| Error::unsupported("typed chain child was not copied"))?;
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
        };
        copied.insert(old, new);
    }
    copied
        .get(&source.root())
        .copied()
        .ok_or_else(|| Error::unsupported("typed query has no root"))
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

    let chain: Vec<_> = typed
        .chain()
        .into_iter()
        .map(|id| typed.node(id))
        .filter(|node| node.kind() != QueryNodeKind::Table)
        .collect();
    let mut matched = 0usize;
    for node in chain {
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
        lower(CompilerInput::new(&ws, &tc), ws.root, def, body)
            .expect("typed lowering")
            .expect("supported direct pipeline")
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

        let (typed, checked) = lower_source(
            "users : query { id = int, age = int } = table \"s\" \"users\"\n\
             q = users & agg { age = group .age, n = count }\n",
            "q",
        );
        assert_eq!(typed.root_kind(), QueryNodeKind::Agg);
        assert_eq!(checked.row().names(), &["age", "n"]);
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
