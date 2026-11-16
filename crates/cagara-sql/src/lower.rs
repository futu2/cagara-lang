//! Relational IR to a sqlglot SELECT. Each IR node either fuses into the
//! current stage or wraps it in a derived table when fusing would change
//! meaning (e.g. filtering after `LIMIT`, or a projection over a `DISTINCT`).
//! A filter after an aggregate is one of the *fusing* cases: it is that
//! aggregate's `HAVING`, because every column the predicate can name is a
//! grouping key or an aggregate value. `place_filter` is the one place that
//! decides among `WHERE`, `HAVING`, `QUALIFY` and a derived table, and it says
//! why each stage shape falls where it does; `docs/PLAN.md`, "Current
//! boundaries", states the rule a reader can act on.

use crate::stage::{and_all, col, ident_style, lower_expr, qualify, Fuse, Stage};
use cagara_hir::ir::{Expr as IrExpr, GroupingShape, JoinKind, Rel, SetKind, Side};
use sqlglot_rust::ast::{
    BinaryOperator, Expr, JoinClause, JoinType, OrderByItem, QuoteStyle, SetOperationStatement,
    SetOperationType, Statement, TableRef, TableSource,
};
use std::collections::{HashMap, HashSet};

/// Has this stage grouped, with nothing after grouping pending?
///
/// The test a filter uses to decide `HAVING` (true) from `WHERE` (false). A
/// window, a `DISTINCT`, a limit or an offset all run *after* grouping, so a
/// filter must stay outside them; without one, the stage's own row is exactly
/// what `HAVING` reads.
fn grouped(st: &Stage) -> bool {
    st.has_agg && !st.has_win && !st.distinct && st.limit.is_none() && st.offset.is_none()
}

/// Deepest `Rel` nesting the lowerer will recurse through for stage kinds that
/// cannot be peeled iteratively. Plain filter chains are handled without this
/// recursion; the bound protects nested relational trees.
const MAX_LOWER_DEPTH: usize = 16;

pub struct Lowerer {
    next: usize,
    next_cte: usize,
    counts: Vec<Use>,
    /// One CTE name per shared sub-pipeline, keyed by the same structural
    /// identity `count_rel` uses. The two must agree: a node counted as shared
    /// but not found here would be hoisted twice, and one found here but not
    /// counted would be wrapped as a CTE reference without a definition.
    cte_names: HashMap<Rel, String>,
    building: Vec<Rel>,
    ctes: Vec<sqlglot_rust::ast::Cte>,
    /// Table names in the query: a generated CTE hides a same-named table
    /// for the whole statement, so its name must avoid all of them.
    tables: HashSet<String>,
    /// Current `rel_inner` nesting, bounded by `MAX_LOWER_DEPTH`.
    depth: usize,
    /// Whether the target has `QUALIFY` (`dialect::supports_qualify`). A filter
    /// over a window folds into the clause when it does, and into a derived table
    /// when it does not.
    has_qualify: bool,
    /// The `At` of the stage being lowered, innermost last.
    ///
    /// A lowering failure used to reach the user as `0:0`, because `Rel` is built
    /// bottom-up and only `Rel::At` remembers where a stage was written. The
    /// lowerer descends that wrapper on its way in, so it is the one place that
    /// knows the answer — and it keeps the *innermost* location on failure, which is
    /// the same rule the schema validator applies (`cagara_hir::schema`): a node
    /// without a location of its own is blamed on the nearest enclosing stage.
    loc: Option<cagara_hir::ir::Loc>,
}

impl Lowerer {
    /// `has_qualify` is a *capability*, not a dialect: the lowerer never matches
    /// on a target, it is told whether this one has the clause. That keeps the
    /// rule "a dialect rule lives in `dialect.rs`" true — `dialect::supports_qualify`
    /// is the only place the list is written.
    pub fn for_rel(rel: &Rel, has_qualify: bool) -> Self {
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
            has_qualify,
            loc: None,
        }
    }

    pub fn take_ctes(self) -> Vec<sqlglot_rust::ast::Cte> {
        self.ctes
    }

    /// Where the lowering was when it failed, if it knew.
    ///
    /// Read after `rel` returns an error. The lowerer keeps the *innermost*
    /// location, so this names the stage the message is about rather than the whole
    /// query.
    pub fn failed_at(&self) -> Option<cagara_hir::ir::Loc> {
        self.loc
    }

    fn alias(&mut self) -> String {
        self.next += 1;
        format!("t{}", self.next)
    }

    /// Rename every output column of a stage, prepending or appending `affix`.
    ///
    /// This mirrors what the checker did with `keyMap (prefix s)` /
    /// `keyMap (suffix s)`: the affix is carried in the `Rel`, so the two agree
    /// by construction rather than by recomputing a rewrite twice.
    ///
    /// As in `select` and `omit`, the rename cannot fold into a preceding
    /// `distinct`, because deduping on the renamed columns is a different
    /// query from deduping on the input row — so wrap first.
    fn rename_all(&mut self, mut st: Stage, affix: &str, prefix: bool) -> Stage {
        if st.distinct {
            st = self.wrap(st);
        }
        st.items = st
            .items
            .into_iter()
            .map(|(name, e)| {
                let renamed = if prefix {
                    format!("{affix}{name}")
                } else {
                    format!("{name}{affix}")
                };
                (renamed, e)
            })
            .collect();
        st
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
                    let n = self.hidden(&st);
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

    /// Name of a hidden pass-through column that cannot collide with a stage's
    /// visible output columns. User schemas may contain names beginning with
    /// `__`, so the generated name must be checked rather than relying on that
    /// prefix being reserved.
    fn hidden(&mut self, st: &Stage) -> String {
        loop {
            self.next += 1;
            let name = format!("__k{}", self.next);
            if !st.items.iter().any(|(n, _)| n == &name) {
                return name;
            }
        }
    }

    /// Turn a stage into a join input. A projection / filter over a plain
    /// table (or over an earlier join) is inlined with qualified columns
    /// instead of becoming a derived table, as long as its filters may move
    /// (`can_place`: an inner join's inputs only — see the join case). A side
    /// the join may null-extend (`null_ext`) is only inlined when it projects
    /// bare columns: a computed column (`1`, `coalesce ..`) must be computed
    /// before the join, or it is not NULL on unmatched rows.
    fn join_input(&mut self, st: Stage, can_place: bool, null_ext: bool) -> JoinInput {
        // A side that may be null-extended may only be inlined when everything it
        // computes stays NULL for an unmatched row; see `null_propagating`.
        let keeps_nulls = st.items.iter().all(|(_, e)| null_propagating(e));
        let fusable = (st.wheres.is_empty() || can_place)
            && (keeps_nulls || !null_ext)
            && st.group_by.is_empty()
            && st.order_by.is_empty()
            && st.limit.is_none()
            && st.offset.is_none()
            && !st.distinct
            && !st.has_agg
            && !st.has_win;
        match st.from {
            TableSource::Table(mut t) if fusable && st.joins.is_empty() => {
                // A table that already carries an alias is one a previous stage
                // has already qualified against, so the alias is kept: renaming it
                // here would leave every item pointing at the old name, because
                // `qualify` only rewrites *unqualified* columns. That produced
                // `SELECT t1.id FROM users AS t3` for a semi join whose input was
                // another semi join — a dangling alias DuckDB refuses with
                // `Referenced table "t1" not found`.
                let alias = match &t.alias {
                    Some(existing) => existing.clone(),
                    None => self.alias(),
                };
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
            if let Some(name) = self.cte_names.get(rel).cloned() {
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
            self.cte_names.insert(rel.clone(), name.clone());
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
        // `Rel::At` is a transparent location wrapper, not a stage: erasure
        // stamps one on every node, so charging it would count each stage
        // twice and halve the real budget. Look through the wrappers before
        // the depth is charged. A node is stamped at most once (`erase`'s
        // `at` leaves an already-located node alone), so this loop is short.
        let mut rel = rel;
        while let Rel::At(_, inner) = rel {
            rel = inner;
        }
        // The recursion below costs stack per `Rel` node. Common filter chains
        // are peeled before reaching this function, while parenthesized
        // relational expressions can still multiply the remaining depth, so
        // retain a defensive diagnostic for those trees.
        let depth = self.depth + 1;
        if depth > MAX_LOWER_DEPTH {
            // The advice used to be "split it with a named definition between
            // the parts", which cannot help: a definition is inlined, so the
            // counter never resets and the split query is exactly as deep. The
            // message now says what the limit is and what actually shortens the
            // spine — fewer stages folding into one another.
            return Err(format!(
                "this query folds more than {MAX_LOWER_DEPTH} stages into one SELECT; \
                 a stage that cannot fold (an aggregate, a window, a limit) starts a new one"
            ));
        }
        self.depth = depth;
        let out = self.rel_peeled(rel);
        self.depth = depth - 1;
        out
    }

    /// Lower a subtree that sits *beside* the current pipeline rather than
    /// continuing it, with the depth counter starting fresh.
    ///
    /// `depth` bounds the left spine of one pipeline: every stage that folds
    /// into the stage below it adds a level, and the bound is what keeps that
    /// recursion off the stack. A join's right operand and a set operation's
    /// two branches are not the continuation of the spine — they become their
    /// own `SELECT`, each with its own spine — so charging them against the
    /// caller's budget measured two unrelated things with one number. A
    /// ten-stage hierarchy on the right of a join and a two-stage union on the
    /// left tripped a bound of sixteen, and the diagnostic's advice ("split it
    /// with a named definition") cannot help, because a definition is inlined
    /// and the counter never resets.
    ///
    /// The recursion is still bounded: each subtree gets the same budget, so a
    /// tree of `n` sibling pipelines may use `n * MAX_LOWER_DEPTH` stack in
    /// total, which is what the stack actually costs, and no single spine may
    /// exceed the bound.
    fn rel_beside(&mut self, rel: &Rel) -> Result<Stage, String> {
        let outer = std::mem::replace(&mut self.depth, 0);
        let out = self.rel(rel);
        self.depth = outer;
        out
    }

    fn rel_peeled(&mut self, rel: &Rel) -> Result<Stage, String> {
        // Peel a run of `where` / `at` stages iteratively.
        //
        // Every arm below starts with `self.rel(r)`, so a pipeline (one node
        // per stage) used one stack frame per stage. On the main thread's
        // 8 MiB stack that survived ~48 stages, but the language server lowers
        // on a spawned thread whose default stack is 2 MiB, where a
        // fifteen-stage pipeline overflowed and took the server down. The
        // peeled stages are re-applied innermost-first, which is exactly the
        // order the recursion produced, so the result is unchanged.
        //
        // `where` and the transparent `at` are peeled here; the remaining kinds
        // still recurse and are bounded by `MAX_LOWER_DEPTH`.
        if matches!(rel, Rel::Where(..) | Rel::At(_, _)) {
            let mut stages: Vec<&IrExpr> = Vec::new();
            let mut base = rel;
            // The outermost `At` seen on the way down, which is the location of the
            // stage this run of `where`s belongs to. The peel is what makes a long
            // pipeline cost one stack frame instead of one per stage; it also means
            // `lower_at` is never reached for these nodes, so the location is
            // recorded here or not at all.
            let mut outer_loc: Option<cagara_hir::ir::Loc> = None;
            loop {
                match base {
                    Rel::At(loc, inner) => {
                        outer_loc = outer_loc.or(Some(*loc));
                        base = inner;
                    }
                    Rel::Where(inner, p) => {
                        stages.push(p);
                        base = inner;
                    }
                    _ => break,
                }
            }
            let saved = self.loc;
            if let Some(loc) = outer_loc {
                self.loc = Some(loc);
            }
            // Keep the location on failure — it is what `compile` reports — and
            // restore it on success, so a later stage is not blamed on this one.
            let lowered = self.rel(base);
            if lowered.is_ok() {
                self.loc = saved;
            }
            let mut st = lowered?;
            // A filter on a stage that has already grouped is a `HAVING`: every
            // column it can name is a grouping key or an aggregate value, and
            // `resolve` turns each into the expression it stands for
            // (`revenue` → `SUM(amount)`), which is what `HAVING` may read. The
            // question is asked of the *stage* — has it grouped, and is nothing
            // that runs after grouping (a window, `DISTINCT`, a limit) waiting —
            // so the answer stays local, like every other fusion decision here.
            //
            // It is not only one derived table saved: `HAVING` is where a
            // grouped filter belongs, so the SQL now says what the pipeline
            // means instead of nesting it.
            for p in stages.into_iter().rev() {
                st = self.place_filter(st, p)?;
            }
            return Ok(st);
        }
        match rel {
            // `At` and `Where` are unreachable here: the peel above takes both
            // at the root of this call and returns. They are kept because the
            // match must still be exhaustive.
            Rel::At(loc, r) => self.lower_at(loc, r),
            Rel::Table {
                schema,
                name,
                columns,
            } => self.lower_table(schema, name, columns),
            Rel::Where(r, p) => self.lower_where(r, p),
            Rel::Select(r, fs) => self.lower_select(r, fs),
            Rel::Update(r, fs) => self.lower_update(r, fs),
            Rel::Omit(r, key) => self.lower_omit(r, key),
            Rel::Prefix(r, affix) => self.lower_prefix(r, affix),
            Rel::Suffix(r, affix) => self.lower_suffix(r, affix),
            Rel::Agg(r, fs) => self.lower_agg(r, fs),
            Rel::Order(r, ks) => self.lower_order(r, ks),
            Rel::Limit(r, n) => self.lower_limit(r, n),
            Rel::Offset(r, n) => self.lower_offset(r, n),
            Rel::Distinct(r) => self.lower_distinct(r),
            Rel::Unnest(r, list) => self.lower_unnest(r, list),
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => self.lower_join(kind, left, right, on),
            Rel::Set { kind, left, right } => self.lower_set(kind, left, right),
            Rel::MarkExists { input, right, on } => self.lower_mark_exists(input, right, on),
        }
    }

    /// `Rel::At` is a location wrapper, not a stage: it lowers to its input, and its
    /// only job here is to say *where* the input was written.
    ///
    /// The location is restored on success and kept on failure. Restoring it on
    /// failure would lose the deepest stage by the time `compile` asks; keeping it is
    /// the same rule `cagara_hir::schema` uses — the innermost stage that has a
    /// location is the one to blame.
    fn lower_at(&mut self, loc: &cagara_hir::ir::Loc, r: &Rel) -> Result<Stage, String> {
        let outer = self.loc;
        self.loc = Some(*loc);
        let out = self.rel(r);
        if out.is_ok() {
            self.loc = outer;
        }
        out
    }

    fn lower_table(
        &mut self,
        schema: &str,
        name: &str,
        columns: &Option<Vec<String>>,
    ) -> Result<Stage, String> {
        let cols = columns.as_ref().ok_or("internal: table without columns")?;
        let t = TableRef {
            catalog: None,
            schema: (!schema.is_empty()).then(|| schema.to_owned()),
            name: name.to_owned(),
            alias: None,
            temporal: None,
            name_quote_style: ident_style(name),
            alias_quote_style: QuoteStyle::None,
        };
        Ok(Stage::new(
            TableSource::Table(t),
            cols.iter().map(|c| (c.clone(), col(None, c))).collect(),
        ))
    }

    /// Where a filter belongs on this stage: `HAVING` after a group, `QUALIFY`
    /// after a window on a target that has the clause, `WHERE` otherwise — and a
    /// derived table when it cannot fold at all.
    ///
    /// One function because there are two callers — the peeled filter chain, which
    /// is the path a pipeline actually takes, and the `Rel::Where` arm, which the
    /// match keeps for exhaustiveness — and they must not disagree. The peel used
    /// to carry its own copy of this decision, which is how the first version of
    /// the `QUALIFY` rule ended up in the arm that never runs.
    fn place_filter(&mut self, mut st: Stage, p: &IrExpr) -> Result<Stage, String> {
        // A filter over a window is `QUALIFY` on a target that has the clause: it
        // runs after the window functions, which is exactly where this filter
        // belongs, so the derived table the general rule would build is not needed.
        // An aggregate is excluded because its filter is `HAVING`, and a `distinct`
        // because a filter before a dedupe is not a filter after one.
        // Paging is excluded, and it is the one that bites: SQL runs `QUALIFY`
        // *before* `LIMIT`, so `select {r = …} & limit 2 & where (.r > 1)` would
        // filter the whole relation and then take two rows, where the pipeline
        // takes two and filters them. Measured on DuckDB before the fix: two rows
        // out where the answer is one.
        let paged = st.limit.is_some() || st.offset.is_some();
        if self.has_qualify && st.has_win && !st.has_agg && !st.distinct && !paged {
            // The predicate is resolved like any other, so a window column becomes
            // the window function itself: `QUALIFY ROW_NUMBER() OVER (…) > 2`. The
            // output *name* would be shorter, and it is wrong here — a projection
            // above this filter can replace the select list and drop the name, and
            // the engine then has nothing to bind it to. Measured: `& where (.r > 2)
            // & select {.id}` emitted `QUALIFY r > 2` against a select list without
            // `r`, and DuckDB refused the statement.
            let e = st.resolve(p)?;
            st.qualify = Some(match st.qualify.take() {
                Some(q) => and_all(vec![q, e]).expect("two conditions"),
                None => e,
            });
            return Ok(st);
        }
        // A filter over an aggregate is that aggregate's `HAVING`: every column
        // the predicate can name is either a grouping key or an aggregate
        // value, and `st.resolve` turns each into the expression it stands for
        // (`revenue` → `SUM(amount)`), which is exactly what `HAVING` may read.
        // Emitting it here keeps one query instead of a derived table.
        //
        // Not when the stage has a limit, an offset, a `DISTINCT` or a window:
        // those change which rows exist or are computed after grouping, and a
        // `HAVING` would filter before them.
        if grouped(&st) {
            let e = st.resolve(p)?;
            st.having = match st.having.take() {
                Some(h) => and_all(vec![h, e]),
                None => Some(e),
            };
            return Ok(st);
        }
        if st.needs_barrier(Fuse::Filter, false) {
            st = self.wrap(st);
        }
        let e = st.resolve(p)?;
        st.wheres.push(e);
        Ok(st)
    }

    fn lower_where(&mut self, r: &Rel, p: &IrExpr) -> Result<Stage, String> {
        let st = self.rel(r)?;
        self.place_filter(st, p)
    }

    /// `unnest xs` — a LATERAL join over the array, one row per element.
    ///
    /// The input is always wrapped first: the expression reads the list column,
    /// and the input's own projection may have dropped or renamed it. The join
    /// is a `CROSS JOIN LATERAL UNNEST(…)`, whose column Postgres and DuckDB
    /// both call `unnest`; the output row names it `value`, which is what the
    /// checker's `merge r { value = a }` promised.
    fn lower_unnest(&mut self, r: &Rel, list: &IrExpr) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        // The lateral reads the list column, so it has to be in scope of the
        // same `FROM`: an identity projection needs no derived table, and the
        // join is one clause cheaper. Anything that changes the rows *after*
        // the projection — a `DISTINCT`, paging, or a grouping whose result the
        // lateral would have to read — keeps the wrap, because the lateral then
        // has to run after it.
        let identity = st
            .items
            .iter()
            .all(|(_, e)| matches!(e, Expr::Column { .. }))
            && !st.distinct
            && !st.has_agg
            && !st.has_win
            && st.limit.is_none()
            && st.offset.is_none();
        if !identity {
            let inner = st;
            let names = inner.names();
            let input_alias = self.alias();
            st = Stage::new(
                TableSource::Subquery {
                    query: Box::new(Statement::Select(inner.into_statement())),
                    alias: Some(input_alias.clone()),
                    alias_quote_style: QuoteStyle::None,
                },
                names
                    .iter()
                    .map(|n| (n.clone(), col(Some(&input_alias), n)))
                    .collect(),
            );
        }
        let expr = st.resolve(list)?;
        let alias = self.alias();
        // A lateral *subquery* rather than a lateral table function: the two
        // engines name a bare `UNNEST(…) AS t2`'s column differently (PostgreSQL
        // calls it `t2`, DuckDB `unnest`), and a projection inside names it once.
        let unnest = Stage::new(
            TableSource::Unnest {
                expr: Box::new(expr),
                alias: None,
                alias_quote_style: QuoteStyle::None,
                with_offset: false,
            },
            vec![("value".to_string(), col(None, "unnest"))],
        );
        let lateral = TableSource::Lateral {
            source: Box::new(TableSource::Subquery {
                query: Box::new(Statement::Select(unnest.into_statement())),
                alias: Some(alias.clone()),
                alias_quote_style: QuoteStyle::None,
            }),
        };
        st.joins.push(JoinClause {
            join_type: JoinType::Cross,
            table: lateral,
            on: None,
            using: vec![],
        });
        st.items
            .push(("value".to_string(), col(Some(&alias), "value")));
        Ok(st)
    }

    fn lower_select(&mut self, r: &Rel, fs: &[(String, IrExpr)]) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        let new_win = fs
            .iter()
            .any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Win(..))));
        // What a projection can and cannot fold over, in two parts.
        //
        // A `DISTINCT` below it is the one that always needs the barrier: dropping
        // a column there changes how many rows are deduped. `(1,'a'), (1,'b')` is
        // two rows after `distinct`, and two rows after projecting `id`; folded it
        // would be one.
        //
        // A `LIMIT` or an `OFFSET` below it does not: those pick *which* rows the
        // stage returns, by an `ORDER BY` whose keys were already resolved to
        // expressions over the input columns — SQL lets an `ORDER BY` name a
        // column the projection does not output, as long as the query is not
        // `DISTINCT`. So the projection may fold whatever it drops.
        //
        // Everything else `needs_barrier` refuses (a window a later stage would
        // have to see, an aggregate) is untouched: `keeps_every_column` only
        // widens the paging case.
        let paged = st.limit.is_some() || st.offset.is_some();
        let shadows = self.shadows_a_sort_key(&st, fs);
        let barrier = if paged && !st.distinct {
            (new_win && (st.has_agg || st.has_win)) || shadows
        } else {
            st.needs_barrier(Fuse::Project, new_win) && (!st.keeps_every_column(fs) || shadows)
        };
        if barrier {
            st = self.wrap(st);
        }
        let items = fs
            .iter()
            .map(|(n, e)| Ok((n.clone(), st.resolve(e)?)))
            .collect::<Result<_, String>>()?;
        let before = st.items.clone();
        st.set_items(items, new_win);
        self.retarget_order(&mut st, &before);
        Ok(st)
    }

    /// Re-aim a sort key after the stage's items changed.
    ///
    /// A sort key is written as the *output* name it names, because SQL resolves a
    /// bare `ORDER BY name` to an output alias before a base column — that is what
    /// makes `select { id = .a, a = .id } & order [asc .id]` order by the output
    /// `id` rather than by the base `a`. But the name is only an output alias while
    /// the select list still has it, and a later stage can take it away or give it
    /// to a different column. Three outcomes, in this order:
    ///
    /// | after the change | the key becomes |
    /// |---|---|
    /// | the name still selects the same column | unchanged |
    /// | the name now selects something else, and the FROM has an alias | `alias.name` |
    /// | the name is gone, or there is no alias to qualify with | the expression the name had before |
    ///
    /// The third row is the one that is easy to miss: `prefix "x_" & order [asc
    /// .x_id] & select { a = .x_id }` emitted `ORDER BY x_id` against a FROM with no
    /// such column, because `x_id` was an output alias of the stage the order was
    /// written on and not of the one that printed it.
    fn retarget_order(&mut self, st: &mut Stage, before: &[(String, Expr)]) {
        let alias = match &st.from {
            sqlglot_rust::ast::TableSource::Subquery { alias, .. } => alias.clone(),
            _ => None,
        };
        for k in &mut st.order_by {
            let Expr::Column {
                name, table: None, ..
            } = &k.expr
            else {
                continue;
            };
            let same = matches!(
                st.items.iter().find(|(n, _)| n == name),
                Some((_, Expr::Column { name: c, table: None, .. })) if c == name
            );
            if same {
                continue;
            }
            if st.items.iter().any(|(n, _)| n == name) {
                if let Some(alias) = &alias {
                    k.expr = col(Some(alias), name);
                    continue;
                }
            }
            if let Some((_, old)) = before.iter().find(|(n, _)| n == name) {
                k.expr = old.clone();
            }
        }
    }

    /// Would folding `fs` into `st` change what one of `st`'s sort keys means?
    ///
    /// A sort key was resolved to an expression over the stage's *input* columns,
    /// and a bare column key is written as that column's name. Once the projection
    /// shares the same `SELECT`, an `ORDER BY name` binds to the *output* `name`
    /// when there is one — so a projection that puts a different column under a
    /// name a sort key uses changes the ordering. `select { name = .id }` under
    /// `order [asc .name]` is exactly that, and it is why the paging fold has to
    /// ask.
    fn shadows_a_sort_key(&self, st: &Stage, fs: &[(String, IrExpr)]) -> bool {
        st.order_by.iter().any(|k| {
            let Expr::Column {
                name, table: None, ..
            } = &k.expr
            else {
                return false;
            };
            fs.iter()
                .find(|(n, _)| n == name)
                .is_some_and(|(_, e)| !matches!(e, IrExpr::Col(Side::Single, c) if c == name))
        })
    }

    fn lower_update(&mut self, r: &Rel, fs: &[(String, IrExpr)]) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        // Like `select`, but the input's columns are kept in place:
        // each updated name takes the new expression, and a name the
        // input does not have is appended. An unlisted column is
        // passed through as itself.
        let new_win = fs
            .iter()
            .any(|(_, e)| e.any(&|x| matches!(x, IrExpr::Win(..))));
        if st.needs_barrier(Fuse::Project, new_win) {
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
        let before = st.items.clone();
        st.set_items(items, new_win);
        self.retarget_order(&mut st, &before);
        Ok(st)
    }

    fn lower_omit(&mut self, r: &Rel, key: &str) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        if st.needs_barrier(Fuse::Omit, false) {
            st = self.wrap(st);
        }
        st.items.retain(|(n, _)| n != key);
        Ok(st)
    }

    fn lower_prefix(&mut self, r: &Rel, affix: &str) -> Result<Stage, String> {
        let st = self.rel(r)?;
        Ok(self.rename_all(st, affix, true))
    }

    fn lower_suffix(&mut self, r: &Rel, affix: &str) -> Result<Stage, String> {
        let st = self.rel(r)?;
        Ok(self.rename_all(st, affix, false))
    }

    fn lower_agg(&mut self, r: &Rel, fs: &[(String, IrExpr)]) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        // `distinct` must dedupe the input rows before they are counted
        // or grouped, so it cannot fold into this stage.
        if st.needs_barrier(Fuse::Aggregate, false) {
            st = self.wrap(st);
        }
        st.order_by.clear();
        // Keys in discovery order, each marked with the subtotal shape it asks
        // for. `group` and `rollup` are the same clause with a different reach:
        // `GROUP BY ROLLUP(a, b)` is `{(a,b), (a), ()}`, so the shape belongs to
        // the stage's key list, not to the expression.
        let mut keys: Vec<(Option<GroupingShape>, &IrExpr)> = Vec::new();
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
            // Plain keys first, then one `ROLLUP`/`CUBE` per shape: a plain key
            // beside a rollup groups *within* it — `GROUP BY x, ROLLUP(a, b)` is
            // `{(x,a,b), (x,a), (x)}` — which is only the meaning if it comes
            // first in the clause.
            let mut plain: Vec<Expr> = Vec::new();
            let mut marked: Vec<(GroupingShape, Vec<Expr>)> = Vec::new();
            for (shape, k) in &keys {
                let e = st.resolve(k)?;
                match shape {
                    None => plain.push(e),
                    Some(shape) => match marked.iter_mut().find(|(s, _)| s == shape) {
                        Some((_, list)) => list.push(e),
                        None => marked.push((*shape, vec![e])),
                    },
                }
            }
            // A constant key does not split groups, and SQL reads
            // `GROUP BY 2` as a position (and rejects `GROUP BY 'x'`).
            // Dropping every key would turn "no rows in, no rows out"
            // into one row, which HAVING keeps.
            let all = keys.len();
            let mut group_by: Vec<Expr> = plain.into_iter().filter(|k| !is_literal(k)).collect();
            for (shape, keys) in marked {
                group_by.push(match shape {
                    GroupingShape::Rollup => Expr::Rollup { exprs: keys },
                    GroupingShape::Cube => Expr::Cube { exprs: keys },
                });
            }
            st.group_by = group_by;
            if st.group_by.is_empty() && all > 0 {
                st.having = Some(crate::stage::template("COUNT(*) > 0", vec![])?);
            }
            st.has_agg = true;
        }
        let items = fs
            .iter()
            .map(|(n, e)| Ok((n.clone(), st.resolve(e)?)))
            .collect::<Result<_, String>>()?;
        st.set_items(items, false);
        Ok(st)
    }

    /// `markExists q on` — the row plus an `exists` column.
    ///
    /// The `EXISTS` is `semiJoin`'s, only as a *value*: both sides are qualified
    /// the same way (which is what makes `.>x` readable inside the subquery) and
    /// the predicate becomes the subquery's `WHERE`. The input is passed through
    /// `join_input` because the outer reference has to be a *qualified* column —
    /// a bare `id` inside the subquery would resolve to the subquery's own table.
    fn lower_mark_exists(
        &mut self,
        input: &Rel,
        right: &Rel,
        on: &IrExpr,
    ) -> Result<Stage, String> {
        let outer_stage = self.rel(input)?;
        let inner_stage = self.rel_beside(right)?;
        let outer = self.join_input(outer_stage, true, false);
        let inner = self.join_input(inner_stage, true, false);
        let predicate = lower_expr(on, &|side, n| match side {
            Side::Left => item(&outer.items, n),
            Side::Right => item(&inner.items, n),
            Side::Single => Err(format!("`markExists` needs `.<{n}` or `.>{n}`")),
        })?;
        let mut subquery = Stage::new(inner.from, inner.items);
        subquery.joins = inner.joins;
        subquery.wheres = inner.wheres;
        subquery.wheres.push(predicate);
        let exists = Expr::Exists {
            subquery: Box::new(sqlglot_rust::Statement::Select(subquery.into_statement())),
            negated: false,
        };
        // `join_input` wraps whatever it cannot inline, so the rebuilt stage is a
        // plain projection over an already-qualified source: nothing of the
        // input's own flags survives, and nothing should — a `limit`, a `distinct`
        // or an aggregate is inside the derived table it wrapped.
        let mut st = Stage::new(outer.from, outer.items);
        st.joins = outer.joins;
        st.wheres = outer.wheres;
        // The column *replaces* one of that name, which is what the row type says
        // (`RowType::with`) and what the prelude promises. Appending instead left
        // two items called `exists` and every later reader took the first — so
        // `& markExists u p & markExists u q & select { a = .exists }` answered
        // `p`'s question and threw `q`'s away, silently.
        match st.items.iter_mut().find(|(n, _)| n == "exists") {
            Some(slot) => slot.1 = exists,
            None => st.items.push(("exists".into(), exists)),
        }
        Ok(st)
    }

    fn lower_order(&mut self, r: &Rel, ks: &[(IrExpr, bool)]) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        // Sorting belongs outside the dedup: a DISTINCT query may
        // order only by its own select list, and an emulated NULLS
        // LAST key cannot appear there.
        if st.needs_barrier(Fuse::Sort, false) {
            st = self.wrap(st);
        }
        let mut order = Vec::new();
        for (k, asc) in ks {
            let expr = self.sort_key(&st, k)?;
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
        // A second `order` sorts rows that are already sorted, so its own keys
        // lead and the earlier ones become tiebreakers: `order [asc a] & order [asc b]`
        // is `ORDER BY b, a`. Replacing them instead made the first `order`
        // silently dead — the same shape as `limit` and `offset` disagreeing
        // about which one runs first.
        let mut merged = order;
        for key in st.order_by.drain(..) {
            if !merged.iter().any(|k| k.expr == key.expr) {
                merged.push(key);
            }
        }
        st.order_by = merged;
        Ok(st)
    }

    /// The expression a sort key is written as.
    ///
    /// A bare column names one of the stage's *output* columns, so it is written
    /// as that name. Resolving it to the item's expression instead is what the
    /// rest of the lowerer does, and it is wrong here: `select { id = .a, a = .id }`
    /// puts `a` in the select list as an alias for a different column, and a
    /// resolved `ORDER BY a` then binds to that alias — ordering by `id` where the
    /// key asked for `a`. Only the renaming case is rewritten; when the item is
    /// the column of the same name the two spellings are the same text.
    fn sort_key(&mut self, st: &Stage, k: &IrExpr) -> Result<Expr, String> {
        let IrExpr::Col(Side::Single, name) = k else {
            return st.resolve(k);
        };
        match st.item(name)? {
            Expr::Column {
                name: source,
                table: None,
                ..
            } if &source != name => Ok(col(None, name)),
            _ => st.resolve(k),
        }
    }

    fn lower_limit(&mut self, r: &Rel, n: &i64) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        if st.needs_barrier(Fuse::Limit, false) {
            st = self.wrap(st);
        }
        st.limit = Some(*n);
        Ok(st)
    }

    fn lower_offset(&mut self, r: &Rel, n: &i64) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        if st.needs_barrier(Fuse::Offset, false) {
            st = self.wrap(st);
        }
        st.offset = Some(*n);
        Ok(st)
    }

    /// The innermost `At` of `r` itself, without descending into its inputs.
    ///
    /// A stage's own location is on the `At` that wraps *it*, and `rel` has already
    /// restored the outer one by the time a check inside a `lower_*` runs. This
    /// recovers it from the node the check is about.
    fn loc_of(r: &Rel) -> Option<cagara_hir::ir::Loc> {
        match r {
            Rel::At(loc, _) => Some(*loc),
            _ => None,
        }
    }

    fn lower_distinct(&mut self, r: &Rel) -> Result<Stage, String> {
        let mut st = self.rel(r)?;
        if st.needs_barrier(Fuse::Distinct, false) {
            st = self.wrap(st);
        }
        // A pending sort whose key the projection dropped cannot survive a
        // `DISTINCT`: SQL refuses `SELECT DISTINCT a … ORDER BY b`, and
        // carrying `b` into the select list would dedupe on `(a, b)` — a
        // different query whose answer keeps rows the user asked to merge.
        // Both readings are wrong, so the pair is refused rather than quietly
        // resolved one way.
        if st
            .order_by
            .iter()
            .any(|k| !st.items.iter().any(|(_, e)| *e == k.expr))
        {
            // The location is already set here: `rel` restored it on success, so
            // this stage's own `At` was consumed below. Re-enter it from the node
            // being lowered, which is where the reader wrote `distinct`.
            self.loc = Self::loc_of(r).or(self.loc);
            return Err(concat!(
                "`distinct` cannot follow an `order` on an expression the output row no ",
                "longer has: order the deduplicated result instead, or keep that expression ",
                "in the projection"
            )
            .to_string());
        }
        st.distinct = true;
        if !st.order_by.is_empty() {
            // `order` before `distinct`: dedupe inside the derived
            // table, sort outside it (see `Rel::Order`). This is not a
            // `Fuse` rule: the stage's own pending sort, not an incoming
            // clause, forces the wrap.
            st = self.wrap(st);
        }
        Ok(st)
    }

    fn lower_join(
        &mut self,
        kind: &JoinKind,
        left: &Rel,
        right: &Rel,
        on: &IrExpr,
    ) -> Result<Stage, String> {
        if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            // A semi/anti join is an existence predicate: it keeps
            // the left relation's cardinality and columns while the
            // right relation only decides whether a matching row
            // exists.
            let left_stage = self.rel(left)?;
            let right_stage = self.rel_beside(right)?;
            // Both sides are inlined where they can be. A semi/anti join keeps
            // the left relation's cardinality and columns, so a filter on the
            // left is a filter on the subject and belongs in the outer `WHERE`;
            // the right side only decides whether a matching row exists, which
            // is what a filter there already means. Neither side is ever
            // null-extended, so there is nothing a derived table protects — and
            // wrapping a plain table produced `FROM (SELECT id, name FROM users)
            // AS t1` for a join that needs no subquery at all.
            let left = self.join_input(left_stage, true, false);
            let right = self.join_input(right_stage, true, false);
            let predicate = lower_expr(on, &|side, n| match side {
                Side::Left => item(&left.items, n),
                Side::Right => item(&right.items, n),
                Side::Single => Err(format!("join predicates need `.<{n}` or `.>{n}`")),
            })?;
            let mut subquery = Stage::new(right.from, right.items);
            subquery.joins = right.joins;
            // The right side's own filters, then the join predicate: both are
            // `WHERE` conditions of the existence check.
            subquery.wheres = right.wheres;
            subquery.wheres.push(predicate);
            let exists = Expr::Exists {
                subquery: Box::new(sqlglot_rust::Statement::Select(subquery.into_statement())),
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
        // The right input is lowered beside the left spine, not as part of it:
        // it becomes its own `SELECT` and has its own stage budget.
        let r = self.rel_beside(right)?;
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
        // The output is the left columns plus the right ones the left does not
        // already have. Reshaping a side before the join (`select`, `update`) is
        // what makes an overlapping name expressible; the join itself is not a
        // place to rename columns.
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
        Ok(st)
    }

    fn lower_set(&mut self, kind: &SetKind, left: &Rel, right: &Rel) -> Result<Stage, String> {
        // Both branches are siblings of the enclosing pipeline, not its
        // continuation: each becomes a `SELECT` on its own side of the set
        // operator, so each gets its own stage budget.
        let left = self.rel_beside(left)?;
        let right = self.rel_beside(right)?;
        let names = left.names();
        let op = match kind {
            SetKind::Union => SetOperationType::Union,
            SetKind::UnionAll => SetOperationType::Union,
            SetKind::Intersect => SetOperationType::Intersect,
            SetKind::Except => SetOperationType::Except,
        };
        let all = matches!(kind, SetKind::UnionAll);
        let stmt = Statement::SetOperation(SetOperationStatement {
            comments: vec![],
            op,
            all,
            left: Box::new(self.branch(left)),
            right: Box::new(self.branch(right)),
            order_by: vec![],
            limit: None,
            offset: None,
            query_options: None,
        });
        let alias = self.alias();
        Ok(Stage::new(
            TableSource::Subquery {
                query: Box::new(stmt),
                alias: Some(alias.clone()),
                alias_quote_style: QuoteStyle::None,
            },
            names
                .iter()
                .map(|n| (n.clone(), col(Some(&alias), n)))
                .collect(),
        ))
    }

    /// One branch of a set operation. A branch that carries its own LIMIT,
    /// OFFSET or ORDER BY cannot be written as a bare SELECT: the set operator's
    /// own tail would apply to the whole operation instead, and
    /// `SELECT ... LIMIT 3 UNION SELECT ... LIMIT 2` is rejected by every engine
    /// while `SELECT ... ORDER BY x UNION SELECT ...` is rejected by SQLite with
    /// "ORDER BY clause should come after UNION not before". Put such a branch
    /// behind a derived table, which is portable. A branch with none of the three
    /// is emitted as is, so the common case stays flat.
    fn branch(&mut self, st: Stage) -> Statement {
        if st.limit.is_none() && st.offset.is_none() && st.order_by.is_empty() {
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
/// share one CTE, so the key is [`Rel`]'s structural `PartialEq`. The CTE name
/// map in [`Lowerer`] is keyed by the same relation and the same derived
/// `Hash`/`Eq`, so the sharing decision and the cache cannot disagree.
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

fn collect_groups<'a>(e: &'a IrExpr, out: &mut Vec<(Option<GroupingShape>, &'a IrExpr)>) {
    match e {
        IrExpr::Group(k) => {
            push_group(None, k, out);
            return;
        }
        IrExpr::Grouping(shape, k) => {
            push_group(Some(*shape), k, out);
            return;
        }
        _ => {}
    }
    for c in e.children() {
        collect_groups(c, out);
    }
}

/// Add a key once. A key repeated with a *different* shape keeps both: `rollup
/// .a` beside `group .a` is a mistake the checker's row equation catches, and
/// silently folding one into the other would hide it here.
fn push_group<'a>(
    shape: Option<GroupingShape>,
    k: &'a IrExpr,
    out: &mut Vec<(Option<GroupingShape>, &'a IrExpr)>,
) {
    if !out.iter().any(|(s, x)| *s == shape && *x == k) {
        out.push((shape, k));
    }
}

/// A bare literal, which SQL may read as a column position.
/// Is this expression NULL whenever the row it reads is absent?
///
/// A join input that may be null-extended can only be inlined when every column it
/// computes would be NULL on an unmatched row — otherwise the derived table is what
/// makes the computation happen *after* the null extension, and inlining it would
/// compute on NULLs and produce a value where the join promises NULL:
///
/// ```text
/// LEFT JOIN (SELECT user_id, amt * 2 AS x FROM orders)   -- inline-able: NULL * 2 is NULL
/// LEFT JOIN (SELECT user_id, coalesce 0 amt AS x …)      -- not: 0 is not NULL
/// ```
///
/// The property is decided structurally. A column is NULL when absent; a literal
/// never is; a binary operator that propagates NULL (arithmetic, comparison,
/// concatenation) is NULL when *either* side is; a function call is not assumed to
/// be, because `coalesce`, `ifnull` and their relatives exist and the template text
/// is all this layer sees.
fn null_propagating(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } => true,
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Boolean(_) | Expr::Null => false,
        Expr::Nested(e) => null_propagating(e),
        Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } => null_propagating(expr),
        Expr::BinaryOp { left, right, op } => {
            matches!(
                op,
                BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo
                    | BinaryOperator::Eq
                    | BinaryOperator::Neq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Concat
            ) && (null_propagating(left) || null_propagating(right))
        }
        _ => false,
    }
}

fn is_literal(e: &Expr) -> bool {
    match e {
        Expr::Number(_) | Expr::StringLiteral(_) | Expr::Boolean(_) | Expr::Null => true,
        Expr::Nested(e) => is_literal(e),
        _ => false,
    }
}
