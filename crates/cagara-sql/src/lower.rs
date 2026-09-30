//! Relational IR to a sqlglot SELECT. Each IR node either fuses into the
//! current stage or wraps it in a derived table when fusing would change
//! meaning (e.g. filtering after aggregation, windows, or LIMIT).

use crate::stage::{and_all, col, ident_style, lower_expr, qualify, Stage};
use cagara_hir::ir::{Expr as IrExpr, JoinKind, Rel, SetKind, Side};
use sqlglot_rust::ast::{
    Expr, JoinClause, JoinType, OrderByItem, QuoteStyle, SetOperationStatement,
    SetOperationType, Statement, TableRef, TableSource,
};
use std::collections::HashMap;

pub struct Lowerer {
    next: usize,
    next_cte: usize,
    counts: Vec<(Rel, usize)>,
    cte_names: HashMap<String, String>,
    building: Vec<Rel>,
    ctes: Vec<sqlglot_rust::ast::Cte>,
}

impl Lowerer {
    pub fn for_rel(rel: &Rel) -> Self {
        let mut counts = Vec::new();
        count_rel(rel, &mut counts);
        Lowerer {
            next: 0,
            next_cte: 0,
            counts,
            cte_names: HashMap::new(),
            building: Vec::new(),
            ctes: Vec::new(),
        }
    }

    pub fn take_ctes(self) -> Vec<sqlglot_rust::ast::Cte> {
        self.ctes
    }

    fn alias(&mut self) -> String {
        self.next += 1;
        format!("t{}", self.next)
    }

    /// Put a stage behind a derived table. Its order carries over to the
    /// outer query, since SQL does not keep the order of a derived table;
    /// the inner query keeps it only when a LIMIT / OFFSET needs it. A sort
    /// key that is not an output column is passed out as a hidden one.
    fn wrap(&mut self, mut st: Stage) -> Stage {
        let names = st.names();
        let paged = st.limit.is_some() || st.offset.is_some();
        let mut outer_order = Vec::new();
        for k in &st.order_by {
            let name = match st.items.iter().find(|(_, e)| *e == k.expr) {
                Some((n, _)) => n.clone(),
                None => {
                    let n = self.hidden();
                    st.items.push((n.clone(), k.expr.clone()));
                    n
                }
            };
            outer_order.push(OrderByItem {
                expr: col(None, &name),
                ..k.clone()
            });
        }
        if !paged {
            st.order_by.clear();
        }
        let from = TableSource::Subquery {
            query: Box::new(sqlglot_rust::Statement::Select(st.into_statement())),
            alias: Some(self.alias()),
            alias_quote_style: QuoteStyle::None,
        };
        let mut out = Stage::new(
            from,
            names.iter().map(|n| (n.clone(), col(None, n))).collect(),
        );
        out.order_by = outer_order;
        out
    }

    /// Name of a hidden pass-through column (users cannot write `__` names).
    fn hidden(&mut self) -> String {
        self.next += 1;
        format!("__k{}", self.next)
    }

    /// Turn a stage into a join input. A projection / filter over a plain
    /// table (or over an earlier join) is inlined with qualified columns
    /// instead of becoming a derived table, as long as its filters may move
    /// (`can_place`: to WHERE for a preserved side, into ON for the right
    /// side of a left join; see the join case). A side the join may
    /// null-extend (`null_ext`) is only inlined when it projects bare
    /// columns: a computed column (`1`, `coalesce ..`) must be computed
    /// before the join, or it is not NULL on unmatched rows.
    fn join_input(&mut self, st: Stage, can_place: bool, null_ext: bool) -> JoinInput {
        let bare = st
            .items
            .iter()
            .all(|(_, e)| matches!(e, Expr::Column { .. }));
        let fusable = (st.wheres.is_empty() || can_place)
            && (bare || !null_ext)
            && st.group_by.is_empty()
            && st.order_by.is_empty()
            && st.limit.is_none()
            && st.offset.is_none()
            && !st.distinct
            && !st.has_agg
            && !st.has_win;
        match st.from {
            TableSource::Table(mut t) if fusable && st.joins.is_empty() => {
                let alias = self.alias();
                t.alias = Some(alias.clone());
                JoinInput {
                    from: TableSource::Table(t),
                    joins: vec![],
                    items: st
                        .items
                        .into_iter()
                        .map(|(n, e)| (n, qualify(e, &alias)))
                        .collect(),
                    wheres: st.wheres.into_iter().map(|w| qualify(w, &alias)).collect(),
                }
            }
            // An earlier join: its columns are already qualified.
            from if fusable && !st.joins.is_empty() => JoinInput {
                from,
                joins: st.joins,
                items: st.items,
                wheres: st.wheres,
            },
            from => self.derived(Stage { from, ..st }),
        }
    }

    /// A join input behind a derived table. A join does not keep the order
    /// of its inputs, so ORDER BY stays only where a LIMIT / OFFSET needs it
    /// (T-SQL rejects it otherwise).
    fn derived(&mut self, mut st: Stage) -> JoinInput {
        if st.limit.is_none() && st.offset.is_none() {
            st.order_by.clear();
        }
        let alias = self.alias();
        let names = st.names();
        JoinInput {
            from: TableSource::Subquery {
                query: Box::new(sqlglot_rust::Statement::Select(st.into_statement())),
                alias: Some(alias.clone()),
                alias_quote_style: QuoteStyle::None,
            },
            joins: vec![],
            items: names
                .iter()
                .map(|n| (n.clone(), col(Some(&alias), n)))
                .collect(),
            wheres: vec![],
        }
    }

    pub fn rel(&mut self, rel: &Rel) -> Result<Stage, String> {
        if self.building.is_empty() && self.is_cte_candidate(rel) {
            let key = format!("{rel:?}");
            if let Some(name) = self.cte_names.get(&key).cloned() {
                return self.cte_stage(&name, rel);
            }
            self.next_cte += 1;
            let name = format!("cagara_cte{}", self.next_cte);
            self.cte_names.insert(key, name.clone());
            self.building.push(rel.clone());
            let body = self.rel_inner(rel)?;
            self.building.pop();
            self.ctes.push(sqlglot_rust::ast::Cte {
                name: name.clone(),
                name_quote_style: QuoteStyle::None,
                columns: vec![],
                query: Box::new(Statement::Select(body.into_statement())),
                materialized: None,
                recursive: false,
            });
            return self.cte_stage(&name, rel);
        }
        self.rel_inner(rel)
    }

    fn rel_inner(&mut self, rel: &Rel) -> Result<Stage, String> {
        Ok(match rel {
            Rel::At(_, r) => self.rel(r)?,
            Rel::Table {
                schema,
                name,
                columns,
            } => {
                let cols = columns.as_ref().ok_or("internal: table without columns")?;
                let t = TableRef {
                    catalog: None,
                    schema: (!schema.is_empty()).then(|| schema.clone()),
                    name: name.clone(),
                    alias: None,
                    temporal: None,
                    name_quote_style: ident_style(name),
                    alias_quote_style: QuoteStyle::None,
                };
                Stage::new(
                    TableSource::Table(t),
                    cols.iter().map(|c| (c.clone(), col(None, c))).collect(),
                )
            }
            Rel::Where(r, p) => {
                let mut st = self.rel(r)?;
                if st.has_agg || st.has_win || st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                let e = st.resolve(p)?;
                st.wheres.push(e);
                st
            }
            Rel::Select(r, fs) => {
                let mut st = self.rel(r)?;
                let new_win = fs
                    .iter()
                    .any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Win(..))));
                if st.limit.is_some()
                    || st.offset.is_some()
                    || (new_win && (st.has_agg || st.has_win))
                {
                    st = self.wrap(st);
                }
                let items = fs
                    .iter()
                    .map(|(n, e)| Ok((n.clone(), st.resolve(e)?)))
                    .collect::<Result<_, String>>()?;
                st.items = items;
                st.has_win |= new_win;
                st
            }
            Rel::Agg(r, fs) => {
                let mut st = self.rel(r)?;
                if st.has_agg || st.has_win || st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                st.order_by.clear();
                let mut keys: Vec<&IrExpr> = Vec::new();
                for (_, e) in fs {
                    collect_groups(e, &mut keys);
                }
                let has_aggfn = fs
                    .iter()
                    .any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Agg(..))));
                if keys.is_empty() && !has_aggfn {
                    // Only constants: force exactly one output row.
                    st.items = vec![(
                        "cagara_n".into(),
                        crate::stage::template("COUNT(*)", vec![])?,
                    )];
                    st.has_agg = true;
                    st = self.wrap(st);
                } else {
                    let keys: Vec<Expr> = keys
                        .iter()
                        .map(|k| st.resolve(k))
                        .collect::<Result<_, _>>()?;
                    // A constant key does not split groups, and SQL reads
                    // `GROUP BY 2` as a position (and rejects `GROUP BY 'x'`).
                    // Dropping every key would turn "no rows in, no rows out"
                    // into one row, which HAVING keeps.
                    let all = keys.len();
                    st.group_by = keys.into_iter().filter(|k| !is_literal(k)).collect();
                    if st.group_by.is_empty() && all > 0 {
                        st.having = Some(crate::stage::template("COUNT(*) > 0", vec![])?);
                    }
                    st.has_agg = true;
                }
                let items = fs
                    .iter()
                    .map(|(n, e)| Ok((n.clone(), st.resolve(e)?)))
                    .collect::<Result<_, String>>()?;
                st.items = items;
                st
            }
            Rel::Order(r, ks) => {
                let mut st = self.rel(r)?;
                if st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                let mut order = Vec::new();
                for (k, asc) in ks {
                    let expr = st.resolve(k)?;
                    // A constant sorts nothing, and `ORDER BY 1` is a position.
                    // NULLs sort last in either direction; `dialect` spells
                    // that per target.
                    if !is_literal(&expr) {
                        order.push(OrderByItem {
                            expr,
                            ascending: *asc,
                            nulls_first: Some(false),
                        });
                    }
                }
                st.order_by = order;
                st
            }
            Rel::Limit(r, n) => {
                let mut st = self.rel(r)?;
                if st.limit.is_some() {
                    st = self.wrap(st);
                }
                st.limit = Some(*n);
                st
            }
            Rel::Offset(r, n) => {
                let mut st = self.rel(r)?;
                if st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                st.offset = Some(*n);
                st
            }
            Rel::Distinct(r) => {
                let mut st = self.rel(r)?;
                if st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                st.distinct = true;
                st
            }
            Rel::KeyMap(r, m) => {
                let mut st = self.rel(r)?;
                let pairs = m.apply(&st.names())?;
                let items = pairs
                    .into_iter()
                    .map(|(old, new)| Ok((new, st.item(&old)?)))
                    .collect::<Result<_, String>>()?;
                st.items = items;
                st
            }
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => {
                if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    // A semi/anti join is an existence predicate: it keeps
                    // the left relation's cardinality and columns while the
                    // right relation only decides whether a matching row
                    // exists.
                    let left_stage = self.rel(left)?;
                    let right_stage = self.rel(right)?;
                    let left = self.derived(left_stage);
                    let right = self.derived(right_stage);
                    let find = |items: &[(String, Expr)], n: &str| {
                        items
                            .iter()
                            .find(|(k, _)| k == n)
                            .map(|(_, e)| e.clone())
                            .ok_or_else(|| format!("internal: join input has no column `{n}`"))
                    };
                    let predicate = lower_expr(on, &|side, n| match side {
                        Side::Left => find(&left.items, n),
                        Side::Right => find(&right.items, n),
                        Side::Single => Err(format!("join predicates need `.<{n}` or `.>{n}`")),
                    })?;
                    let mut subquery = Stage::new(right.from, right.items);
                    subquery.joins = right.joins;
                    subquery.wheres.push(predicate);
                    let exists = Expr::Exists {
                        subquery: Box::new(sqlglot_rust::Statement::Select(
                            subquery.into_statement(),
                        )),
                        negated: matches!(kind, JoinKind::Anti),
                    };
                    let mut st = Stage::new(left.from, left.items);
                    st.joins = left.joins;
                    st.wheres = left.wheres;
                    st.wheres.push(exists);
                    return Ok(st);
                }
                // Where an inlined input's filters may go without changing
                // the result: a filter on a side whose unmatched rows are not
                // kept can move to WHERE; the right side of a left join can
                // take its filter into ON; the other cases need a derived table.
                // Filters of the preserved side move to WHERE; those of a left
                // join's right side move into ON. The null-extended side of a
                // right / full join keeps a derived table.
                let (left_ok, right_ok) = match kind {
                    JoinKind::Inner | JoinKind::Left => (true, true),
                    JoinKind::Right => (false, true),
                    JoinKind::Full => (false, false),
                    JoinKind::Semi | JoinKind::Anti => unreachable!(),
                };
                // Sides whose unmatched rows are kept with NULLs for the other.
                let (left_null, right_null) = match kind {
                    JoinKind::Inner => (false, false),
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                    JoinKind::Semi | JoinKind::Anti => unreachable!(),
                };
                let l = self.rel(left)?;
                let r = self.rel(right)?;
                let l = self.join_input(l, left_ok, left_null);
                // The right input must be a single table or derived table.
                let r = match self.join_input(r, right_ok, right_null) {
                    ji if ji.joins.is_empty() => ji,
                    ji => self.rewrap(ji),
                };
                let find = |items: &[(String, Expr)], n: &str| {
                    items
                        .iter()
                        .find(|(k, _)| k == n)
                        .map(|(_, e)| e.clone())
                        .ok_or_else(|| format!("internal: join input has no column `{n}`"))
                };
                let pred = lower_expr(on, &|side, n| match side {
                    Side::Left => find(&l.items, n),
                    Side::Right => find(&r.items, n),
                    Side::Single => Err(format!("join predicates need `.<{n}` or `.>{n}`")),
                })?;
                let items = cagara_hir::rules::join_columns(&l.items, &r.items);
                let mut wheres = l.wheres;
                let on = match kind {
                    JoinKind::Inner | JoinKind::Right => {
                        wheres.extend(r.wheres);
                        pred
                    }
                    _ if r.wheres.is_empty() => pred,
                    _ => and_all(std::iter::once(pred).chain(r.wheres)).expect("non-empty"),
                };
                let mut st = Stage::new(l.from, items);
                st.joins = l.joins;
                st.wheres = wheres;
                let join_type = match kind {
                    JoinKind::Inner => JoinType::Inner,
                    JoinKind::Left => JoinType::Left,
                    JoinKind::Right => JoinType::Right,
                    JoinKind::Full => JoinType::Full,
                    JoinKind::Semi | JoinKind::Anti => unreachable!(),
                };
                st.joins.push(JoinClause {
                    join_type,
                    table: r.from,
                    on: Some(on),
                    using: vec![],
                });
                st
            }
            Rel::Set { kind, left, right } => {
                let left = self.rel(left)?;
                let right = self.rel(right)?;
                let names = left.names();
                let op = match kind {
                    SetKind::Union => SetOperationType::Union,
                    SetKind::Intersect => SetOperationType::Intersect,
                    SetKind::Except => SetOperationType::Except,
                };
                let stmt = Statement::SetOperation(SetOperationStatement {
                    comments: vec![],
                    op,
                    all: false,
                    left: Box::new(Statement::Select(left.into_statement())),
                    right: Box::new(Statement::Select(right.into_statement())),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    query_options: None,
                });
                let alias = self.alias();
                Stage::new(
                    TableSource::Subquery {
                        query: Box::new(stmt),
                        alias: Some(alias.clone()),
                        alias_quote_style: QuoteStyle::None,
                    },
                    names
                        .iter()
                        .map(|n| (n.clone(), col(Some(&alias), n)))
                        .collect(),
                )
            }
        })
    }

    fn is_cte_candidate(&self, rel: &Rel) -> bool {
        if matches!(rel.bare(), Rel::Table { .. }) {
            return false;
        }
        self.counts
            .iter()
            .any(|(candidate, count)| count > &1 && candidate == rel)
    }

    fn cte_stage(&self, name: &str, rel: &Rel) -> Result<Stage, String> {
        let columns = cagara_hir::schema::schema(rel)?;
        let table = TableRef {
            catalog: None,
            schema: None,
            name: name.to_string(),
            alias: None,
            temporal: None,
            name_quote_style: ident_style(name),
            alias_quote_style: QuoteStyle::None,
        };
        Ok(Stage::new(
            TableSource::Table(table),
            columns
                .iter()
                .map(|n| (n.clone(), col(None, n)))
                .collect(),
        ))
    }
}

fn count_rel(rel: &Rel, counts: &mut Vec<(Rel, usize)>) {
    if let Some((_, count)) = counts.iter_mut().find(|(candidate, _)| candidate == rel) {
        *count += 1;
    } else {
        counts.push((rel.clone(), 1));
    }
    for child in rel.children() {
        count_rel(child, counts);
    }
}

/// A join input: its FROM source (plus joins when it is an earlier join),
/// output columns as qualified expressions, and filters still to place.
struct JoinInput {
    from: TableSource,
    joins: Vec<JoinClause>,
    items: Vec<(String, Expr)>,
    wheres: Vec<Expr>,
}

impl Lowerer {
    /// Put a multi-table join input behind a derived table.
    fn rewrap(&mut self, ji: JoinInput) -> JoinInput {
        let mut st = Stage::new(ji.from, ji.items);
        st.joins = ji.joins;
        st.wheres = ji.wheres;
        self.derived(st)
    }
}

fn collect_groups<'a>(e: &'a IrExpr, out: &mut Vec<&'a IrExpr>) {
    if let IrExpr::Group(k) = e {
        if !out.contains(&k.as_ref()) {
            out.push(k);
        }
        return;
    }
    for c in e.children() {
        collect_groups(c, out);
    }
}

/// A bare literal, which SQL may read as a column position.
fn is_literal(e: &Expr) -> bool {
    match e {
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Boolean(_) | Expr::Null => true,
        Expr::Nested(e) => is_literal(e),
        _ => false,
    }
}
