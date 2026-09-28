//! Relational IR to a sqlglot SELECT. Each IR node either fuses into the
//! current stage or wraps it in a derived table when fusing would change
//! meaning (e.g. filtering after aggregation, windows, or LIMIT).

use crate::stage::{col, lower_expr, Stage};
use cagara_hir::ir::{Expr as IrExpr, JoinKind, Rel, Side};
use sqlglot_rust::ast::{Expr, JoinClause, JoinType, OrderByItem, QuoteStyle, TableRef, TableSource};

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
        Stage::new(from, names.iter().map(|n| (n.clone(), col(None, n))).collect())
    }

    /// Turn a stage into an aliased join input; returns (source, alias, columns).
    fn join_input(&mut self, st: Stage) -> (TableSource, String, Vec<String>) {
        let alias = self.alias();
        let names = st.names();
        let src = if st.is_bare_table() {
            match st.from {
                TableSource::Table(mut t) => {
                    t.alias = Some(alias.clone());
                    TableSource::Table(t)
                }
                _ => unreachable!("is_bare_table checked the source"),
            }
        } else {
            TableSource::Subquery {
                query: Box::new(sqlglot_rust::Statement::Select(st.into_statement())),
                alias: Some(alias.clone()),
                alias_quote_style: QuoteStyle::None,
            }
        };
        (src, alias, names)
    }

    pub fn rel(&mut self, rel: &Rel) -> Result<Stage, String> {
        Ok(match rel {
            Rel::Table { schema, name, columns } => {
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
                Stage::new(TableSource::Table(t), cols.iter().map(|c| (c.clone(), col(None, c))).collect())
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
                let new_win = fs.iter().any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Win(..))));
                if st.limit.is_some() || st.offset.is_some() || (new_win && (st.has_agg || st.has_win)) {
                    st = self.wrap(st);
                }
                let items = fs.iter().map(|(n, e)| Ok((n.clone(), st.resolve(e)?))).collect::<Result<_, String>>()?;
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
                let has_aggfn = fs.iter().any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Agg(..))));
                if keys.is_empty() && !has_aggfn {
                    // Only constants: force exactly one output row.
                    st.items = vec![("cagara_n".into(), crate::stage::template("COUNT(*)", vec![])?)];
                    st.has_agg = true;
                    st = self.wrap(st);
                } else {
                    st.group_by = keys.iter().map(|k| st.resolve(k)).collect::<Result<_, _>>()?;
                    st.has_agg = true;
                }
                let items = fs.iter().map(|(n, e)| Ok((n.clone(), st.resolve(e)?))).collect::<Result<_, String>>()?;
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
                    .map(|(k, asc)| Ok(OrderByItem { expr: st.resolve(k)?, ascending: *asc, nulls_first: None }))
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
                let items = pairs.into_iter().map(|(old, new)| Ok((new, st.item(&old)?))).collect::<Result<_, String>>()?;
                st.items = items;
                st
            }
            Rel::Join { kind, left, right, on } => {
                let l = self.rel(left)?;
                let r = self.rel(right)?;
                let (lsrc, la, lnames) = self.join_input(l);
                let (rsrc, ra, rnames) = self.join_input(r);
                let on = lower_expr(on, &|side, n| match side {
                    Side::Left => Ok(col(Some(&la), n)),
                    Side::Right => Ok(col(Some(&ra), n)),
                    Side::Single => Err(format!("join predicates need `.<{n}` or `.>{n}`")),
                })?;
                let mut items: Vec<(String, Expr)> = lnames.iter().map(|n| (n.clone(), col(Some(&la), n))).collect();
                items.extend(rnames.iter().filter(|n| !lnames.contains(n)).map(|n| (n.clone(), col(Some(&ra), n))));
                let mut st = Stage::new(lsrc, items);
                let join_type = match kind {
                    JoinKind::Inner => JoinType::Inner,
                    JoinKind::Left => JoinType::Left,
                    JoinKind::Right => JoinType::Right,
                    JoinKind::Full => JoinType::Full,
                };
                st.joins.push(JoinClause { join_type, table: rsrc, on: Some(on), using: vec![] });
                st
            }
        })
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
