//! Relational IR to a sqlglot SELECT. Each IR node either fuses into the
//! current stage or wraps it in a derived table when fusing would change
//! meaning (e.g. filtering after aggregation, windows, or LIMIT).

use crate::stage::{and_all, col, ident_style, lower_expr, qualify, Stage};
use cagara_hir::ir::{Expr as IrExpr, JoinKind, Rel, SetKind, Side};
use sqlglot_rust::ast::{
    Expr, JoinClause, JoinType, OrderByItem, QuoteStyle, SetOperationStatement, SetOperationType,
    Statement, TableRef, TableSource,
};
use std::collections::{HashMap, HashSet};

/// Deepest `Rel` nesting the lowerer will recurse through; see `rel_inner`.
const MAX_LOWER_DEPTH: usize = 16;

pub struct Lowerer {
    next: usize,
    next_cte: usize,
    counts: Vec<Use>,
    cte_names: HashMap<String, String>,
    building: Vec<Rel>,
    ctes: Vec<sqlglot_rust::ast::Cte>,
    /// Table names in the query: a generated CTE hides a same-named table
    /// for the whole statement, so its name must avoid all of them.
    tables: HashSet<String>,
    /// Current `rel_inner` nesting, bounded by `MAX_LOWER_DEPTH`.
    depth: usize,
}

impl Lowerer {
    pub fn for_rel(rel: &Rel) -> Self {
        let mut counts = Vec::new();
        count_rel(rel, &mut counts);
        let mut tables = HashSet::new();
        collect_tables(rel, &mut tables);
        Lowerer {
            next: 0,
            next_cte: 0,
            counts,
            cte_names: HashMap::new(),
            building: Vec::new(),
            ctes: Vec::new(),
            tables,
            depth: 0,
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
    /// (`can_place`: an inner join's inputs only — see the join case). A side
    /// the join may null-extend (`null_ext`) is only inlined when it projects
    /// bare columns: a computed column (`1`, `coalesce ..`) must be computed
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

    /// Lower one node.
    ///
    /// A relation used more than once is hoisted into a CTE so its work is
    /// shared. That is only sound when both uses want the same SQL. A use that
    /// *fuses* — a projection, an update, a filter, an order — rewrites the
    /// stage it is handed, folding a window or aggregate into it or wrapping
    /// it, and the CTE body was built once without that context. Sharing it
    /// would hand that caller the bare body, silently dropping the folding.
    ///
    /// So a node whose own lowering depends on how its result will be used is
    /// never hoisted; it is lowered in place. `count_rel` records, per node,
    /// whether any use fuses into it, and `is_cte_candidate` requires that
    /// none does.
    pub fn rel(&mut self, rel: &Rel) -> Result<Stage, String> {
        if self.building.is_empty() && self.is_cte_candidate(rel) {
            let key = format!("{rel:?}");
            if let Some(name) = self.cte_names.get(&key).cloned() {
                return self.cte_stage(&name, rel);
            }
            self.next_cte += 1;
            let mut name = format!("cagara_cte{}", self.next_cte);
            // Unquoted identifiers are case-folded by most engines, so
            // compare case-insensitively; skipping an extra number is free.
            while self.tables.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                self.next_cte += 1;
                name = format!("cagara_cte{}", self.next_cte);
            }
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
        // The recursion below costs stack per `Rel` node, and a node chain is
        // as long as the program's stage count. The parser bounds one
        // *pipeline* (`MAX_PIPE_CHAIN`), but a parenthesized group restarts
        // that count, so nesting multiplies the total and the bound does not
        // hold here. This is the lowerer's own bound, on the recursion it
        // actually performs.
        //
        // The value is set by the *smallest* stack the lowerer runs on: a
        // spawned thread gets 2 MiB by default (the language server's, and the
        // test harness's), and an unoptimised build uses far more stack per
        // frame than a release one, overflowing there at around twenty levels.
        // 16 leaves headroom on that configuration. It is well past any
        // hand-written query — the prelude's largest pipeline is single
        // digits — but it is a real ceiling: raising it means peeling the
        // remaining single-input stage kinds iteratively, the way `where`
        // already is.
        let depth = self.depth + 1;
        if depth > MAX_LOWER_DEPTH {
            return Err(format!(
                "this query nests more than {MAX_LOWER_DEPTH} stages deep; \
                 split it with a named definition between the parts"
            ));
        }
        self.depth = depth;
        let out = self.rel_peeled(rel);
        self.depth = depth - 1;
        out
    }

    fn rel_peeled(&mut self, rel: &Rel) -> Result<Stage, String> {
        // Peel a run of `where` / `limit` / `offset` / `distinct` / `at`
        // stages iteratively.
        //
        // Every arm below starts with `self.rel(r)`, so a pipeline (one node
        // per stage) used one stack frame per stage. On the main thread's
        // 8 MiB stack that survived ~48 stages, but the language server lowers
        // on a spawned thread whose default stack is 2 MiB, where a
        // fifteen-stage pipeline overflowed and took the server down. The
        // peeled stages are re-applied innermost-first, which is exactly the
        // order the recursion produced, so the result is unchanged.
        //
        // Only `where` is peeled here: it is what a pipeline is built from.
        // The remaining kinds still recurse, and are bounded by
        // `MAX_LOWER_DEPTH` rather than the parser's per-pipeline budget,
        // which nesting multiplies.
        if matches!(rel, Rel::Where(..)) {
            let mut stages: Vec<(&Rel, &IrExpr)> = Vec::new();
            let mut base = rel;
            while let Rel::Where(inner, p) = base {
                stages.push((base, p));
                base = inner;
            }
            let mut st = self.rel(base)?;
            for (node, p) in stages.into_iter().rev() {
                if st.has_agg || st.has_win || st.limit.is_some() || st.offset.is_some() {
                    st = self.wrap(st);
                }
                let e = st.resolve(p)?;
                st.wheres.push(e);
                let _ = node;
            }
            return Ok(st);
        }
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
                // A projection after `distinct` would dedupe on the projected
                // columns instead of the input row; dedupe first.
                if st.limit.is_some()
                    || st.offset.is_some()
                    || st.distinct
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
            Rel::Update(r, fs) => {
                let mut st = self.rel(r)?;
                // Like `select`, but the input's columns are kept in place:
                // each updated name takes the new expression, and a name the
                // input does not have is appended. An unlisted column is
                // passed through as itself.
                let new_win = fs
                    .iter()
                    .any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Win(..))));
                // As in `select`: a projection after `distinct` would dedupe
                // on the projected columns instead of the input row, and a
                // window must not fold into a stage that already has one.
                if st.limit.is_some()
                    || st.offset.is_some()
                    || st.distinct
                    || (new_win && (st.has_agg || st.has_win))
                {
                    st = self.wrap(st);
                }
                let input: Vec<(String, ())> = st.names().into_iter().map(|c| (c, ())).collect();
                let out = cagara_hir::schema::merge_columns(&input, fs)?;
                let mut items = Vec::with_capacity(out.len());
                for name in out {
                    let e = match fs.iter().find(|(n, _)| *n == name) {
                        Some((_, e)) => st.resolve(e)?,
                        None => st.item(&name)?,
                    };
                    items.push((name, e));
                }
                st.items = items;
                st.has_win |= new_win;
                st
            }
            Rel::Omit(r, key) => {
                let mut st = self.rel(r)?;
                // As in `select`: a projection after `distinct` would dedupe on
                // the projected columns instead of the input row.
                if st.distinct {
                    st = self.wrap(st);
                }
                st.items.retain(|(n, _)| n != key);
                st
            }
            Rel::MapKeys(r, pattern, replacement) => {
                let mut st = self.rel(r)?;
                // A rename is semantically transparent, but a projection after
                // `distinct` still has to move outside it (see `select`).
                if st.distinct {
                    st = self.wrap(st);
                }
                let names: Vec<String> = st.items.iter().map(|(n, _)| n.clone()).collect();
                let cols = cagara_hir::schema::map_columns(&names, pattern, replacement)?;
                for (item, new) in st.items.iter_mut().zip(cols) {
                    item.0 = new;
                }
                st
            }
            Rel::Agg(r, fs) => {
                let mut st = self.rel(r)?;
                // `distinct` must dedupe the input rows before they are
                // counted or grouped, so it cannot fold into this stage.
                if st.has_agg
                    || st.has_win
                    || st.distinct
                    || st.limit.is_some()
                    || st.offset.is_some()
                {
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
                // Sorting belongs outside the dedup: a DISTINCT query may
                // order only by its own select list, and an emulated NULLS
                // LAST key cannot appear there.
                if st.limit.is_some() || st.offset.is_some() || st.distinct {
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
                if !st.order_by.is_empty() {
                    // `order` before `distinct`: dedupe inside the derived
                    // table, sort outside it (see `Rel::Order`).
                    st = self.wrap(st);
                }
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
                    let predicate = lower_expr(on, &|side, n| match side {
                        Side::Left => item(&left.items, n),
                        Side::Right => item(&right.items, n),
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
                // `can_place` says a side's filters may leave its input and be
                // placed by the caller. Only an inner join may do that: a
                // filter there changes only which rows match, and both sides
                // are discarded equally, so it does not matter which side of
                // the join it sits on.
                //
                // Every other kind keeps each side's filters in that side's
                // derived table, because a join input is a *relation* and its
                // filter decides which rows it contributes. Hoisting is wrong
                // in both directions:
                //
                //   * into ON widens the input back to the unfiltered
                //     relation. `orders & rightJoin (users & where (.id > 1))`
                //     would regain user 1 as an unmatched right row, and
                //     `orders & fullJoin (users & where (.id > 2))` would
                //     regain users 1 and 2.
                //   * into WHERE drops the null-extended rows. For a right
                //     join, `WHERE t1.amount > 4.5` removes not only the left
                //     rows that fail it but also every row where the left side
                //     is NULL — that is, the unmatched right rows the join
                //     exists to keep.
                //
                //   kind   left_ok  right_ok
                //   inner  true     true
                //   left   false    false
                //   right  false    false
                //   full   false    false
                let (left_ok, right_ok) = match kind {
                    JoinKind::Inner => (true, true),
                    JoinKind::Left | JoinKind::Right | JoinKind::Full => (false, false),
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
                let pred = lower_expr(on, &|side, n| match side {
                    Side::Left => item(&l.items, n),
                    Side::Right => item(&r.items, n),
                    Side::Single => Err(format!("join predicates need `.<{n}` or `.>{n}`")),
                })?;
                let items = cagara_hir::rules::join_columns(&l.items, &r.items);
                let mut wheres = l.wheres;
                // Only an inner join can have extracted filters to place, and
                // there either side may go to WHERE. For every other kind both
                // sides keep their filters in a derived table, so `r.wheres`
                // is empty and the predicate stands alone.
                let on = match kind {
                    JoinKind::Inner => {
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
                    left: Box::new(self.branch(left)),
                    right: Box::new(self.branch(right)),
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

    /// One branch of a set operation. A branch with its own LIMIT / OFFSET
    /// cannot be written as a bare SELECT: the set operator's own tail would
    /// apply to the whole operation instead, and `SELECT ... LIMIT 3 UNION
    /// SELECT ... LIMIT 2` is rejected by every engine. Put such a branch
    /// behind a derived table, which is portable. A plain branch is emitted
    /// as is, so the common case stays flat.
    fn branch(&mut self, st: Stage) -> Statement {
        if st.limit.is_none() && st.offset.is_none() {
            return Statement::Select(st.into_statement());
        }
        let alias = self.alias();
        let names = st.names();
        let inner = Statement::Select(st.into_statement());
        let outer = Stage::new(
            TableSource::Subquery {
                query: Box::new(inner),
                alias: Some(alias.clone()),
                alias_quote_style: QuoteStyle::None,
            },
            names
                .iter()
                .map(|n| (n.clone(), col(Some(&alias), n)))
                .collect(),
        );
        Statement::Select(outer.into_statement())
    }

    /// Is this node worth sharing as a CTE? It must occur more than once, and
    /// every occurrence must be in a position that does not fuse into it —
    /// otherwise one call site's shape would be imposed on the other.
    fn is_cte_candidate(&self, rel: &Rel) -> bool {
        if matches!(rel.bare(), Rel::Table { .. }) {
            return false;
        }
        self.counts
            .iter()
            .any(|u| u.count > 1 && !u.fused && &u.rel == rel)
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
            columns.iter().map(|n| (n.clone(), col(None, n))).collect(),
        ))
    }
}

/// The expression of column `n` in a join input, or an internal error: a
/// join predicate may only name columns of the two sides.
fn item(items: &[(String, Expr)], n: &str) -> Result<Expr, String> {
    items
        .iter()
        .find(|(k, _)| k == n)
        .map(|(_, e)| e.clone())
        .ok_or_else(|| format!("internal: join input has no column `{n}`"))
}

/// A node worth sharing as a CTE: how often it occurs, and whether every
/// occurrence is reached in a context-free position.
struct Use {
    rel: Rel,
    count: usize,
    /// True when some occurrence sits where the parent fuses into it, which
    /// makes its SQL depend on that parent. Such a node cannot be shared.
    fused: bool,
}

/// Record every distinct `Rel` shape, how often it occurs, and whether any
/// occurrence is used by a parent that fuses into it.
///
/// Two separately written but identical sub-pipelines occur "twice" and may
/// share one CTE, so the key is structural equality.
///
/// An explicit walk, not recursion: the IR is a chain as deep as the program's
/// stage count, and nesting multiplies that past what the parser bounds, so a
/// recursive walk overflowed the stack before lowering could report anything.
fn count_rel(rel: &Rel, uses: &mut Vec<Use>) {
    let mut stack = vec![(rel, false)];
    while let Some((r, fused_by_parent)) = stack.pop() {
        match uses.iter_mut().find(|u| &u.rel == r) {
            Some(u) => {
                u.count += 1;
                u.fused |= fused_by_parent;
            }
            None => uses.push(Use {
                rel: r.clone(),
                count: 1,
                fused: fused_by_parent,
            }),
        }
        // A child of a fusing parent is itself fused: the parent rewrites or
        // wraps the stage the child produced.
        let fuses = fuses_into_child(r);
        stack.extend(r.children().into_iter().map(|c| (c, fuses)));
    }
}

/// Does this node's lowering rewrite the stage its input produced, so that the
/// input's SQL depends on this parent?
///
/// Almost every stage kind does. A `select` *replaces* its input's columns in
/// the same SELECT, an `agg` groups in place, a `where` appends to the WHERE,
/// an `order` sets the ORDER BY, and so on: the input's work is not
/// materialised, it is continued. Only the kinds that must build a new `FROM`
/// with their inputs as subqueries — a set operation, and a join on the side it
/// cannot inline — give their inputs a context-free life of their own.
fn fuses_into_child(rel: &Rel) -> bool {
    !matches!(rel.bare(), Rel::Table { .. } | Rel::Set { .. })
}

/// Every table named in the query, walked without recursion for the same
/// reason as [`count_rel`].
fn collect_tables(rel: &Rel, tables: &mut HashSet<String>) {
    let mut stack = vec![rel];
    while let Some(r) = stack.pop() {
        if let Rel::Table { name, .. } = r {
            tables.insert(name.clone());
        }
        stack.extend(r.children());
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
