//! Relational IR to a sqlglot SELECT. Each IR node either fuses into the
//! current stage or wraps it in a derived table when fusing would change
//! meaning (e.g. filtering after aggregation, windows, or LIMIT).

use crate::stage::{and_all, col, lower_expr, qualify, Stage};
use cagara_hir::ir::{Expr as IrExpr, JoinKind, Rel, Side};
use sqlglot_rust::ast::{
    Expr, JoinClause, JoinType, OrderByItem, QuoteStyle, TableRef, TableSource,
};

#[derive(Default)]
pub struct Lowerer {
    next: usize,
}

impl Lowerer {
    fn alias(&mut self) -> String {
        self.next += 1;
        format!("t{}", self.next)
    }

    fn wrap(&mut self, st: Stage) -> Stage {
        let names = st.names();
        let from = TableSource::Subquery {
            query: Box::new(sqlglot_rust::Statement::Select(st.into_statement())),
            alias: Some(self.alias()),
            alias_quote_style: QuoteStyle::None,
        };
        Stage::new(
            from,
            names.iter().map(|n| (n.clone(), col(None, n))).collect(),
        )
    }

    /// Turn a stage into a join input. A projection / filter over a plain
    /// table (or over an earlier join) is inlined with qualified columns
    /// instead of becoming a derived table, as long as its filters may move
    /// (`can_place`: to WHERE for a preserved side, into ON for the right
    /// side of a left join; see the join case).
    fn join_input(&mut self, st: Stage, can_place: bool) -> JoinInput {
        let fusable = (st.wheres.is_empty() || can_place)
            && st.group_by.is_empty()
            && st.order_by.is_empty()
            && st.limit.is_none()
            && st.offset.is_none()
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

    /// A join input behind a derived table.
    fn derived(&mut self, st: Stage) -> JoinInput {
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
                    name_quote_style: QuoteStyle::None,
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
                    st.group_by = keys
                        .iter()
                        .map(|k| st.resolve(k))
                        .collect::<Result<_, _>>()?;
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
                let order = ks
                    .iter()
                    .map(|(k, asc)| {
                        Ok(OrderByItem {
                            expr: st.resolve(k)?,
                            ascending: *asc,
                            nulls_first: None,
                        })
                    })
                    .collect::<Result<_, String>>()?;
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
                };
                let l = self.rel(left)?;
                let r = self.rel(right)?;
                let l = self.join_input(l, left_ok);
                // The right input must be a single table or derived table.
                let r = match self.join_input(r, right_ok) {
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
                let lnames: Vec<&String> = l.items.iter().map(|(n, _)| n).collect();
                let mut items: Vec<(String, Expr)> = l.items.clone();
                items.extend(
                    r.items
                        .iter()
                        .filter(|(n, _)| !lnames.contains(&n))
                        .cloned(),
                );
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
                };
                st.joins.push(JoinClause {
                    join_type,
                    table: r.from,
                    on: Some(on),
                    using: vec![],
                });
                st
            }
        })
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
