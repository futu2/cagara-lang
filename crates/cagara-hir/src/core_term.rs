//! `CoreTerm`: the explicit relational constructors the evaluator calls
//! instead of `Prim` dispatch.
//!
//! Today a stage primitive builds a `Rel` node directly, so the *only* record
//! that a `where` was built is a `Rel` node, and every semantic rule about it
//! has to be re-derived later by a walk over that node. Here each primitive
//! has a named constructor that states its rule and returns `Result`.
//!
//! ```text
//! Prim::Where => CoreTerm::where_(query(next())?, lift(next())?)
//! ```
//!
//! instead of
//!
//! ```text
//! Prim::Where => Value::Query(Rel::Where(Box::new(query(next())?), pred))
//! ```
//!
//! The difference is the point: the constructor is where the rule is stated.
//!
//! `CoreTerm` is a plain owned tree — no `Rc`, no closures, no environments,
//! no partial applications. User lambdas are still run by the evaluator; what
//! survives into a `CoreTerm` is the saturated application. The constructors
//! are *infallible where the shape cannot be wrong* (`tpl`, `col`, `dir`,
//! `set`, `at`) and `Result` where a rule applies.
//!
//! Erasure ([`erase_core`]) produces exactly the `Rel` the evaluator builds
//! today, so the SQL backend and `schema::schema` keep working unchanged.
//!
//! Note what this module does **not** do: it never wraps a node in
//! [`Rel::At`] on its own. Today the only producer of `Rel::At` is
//! `eval.rs`'s application branch, so [`CoreTerm::at`] is the only way one
//! appears and the caller decides where. Erasure therefore keeps the
//! evaluator's exact tree.

use crate::checked::{self, CheckedExpr, CheckedQuery, WinSpecChecked};
use crate::core::Error;
use crate::ir::{self, Bound, Expr, Frame, JoinKind, Lit, Loc, Phase, Rel, SetKind, Side, WinSpec};
use crate::rules;
use cagara_syntax::ast::Span;

/// A relational or scalar term built by an explicit constructor.
///
/// The variants mirror `Rel` and `Expr` case for case, plus the scalar leaves
/// (`Dir`, `Frame`, `Bound`) the evaluator passes around before they are
/// consumed by `order` or `win`.
#[derive(Debug, Clone, PartialEq)]
pub enum CoreTerm {
    // ── relations ──
    Table {
        schema: String,
        name: String,
        /// `None` until `attach_schema` fills it from the definition's type.
        columns: Option<Vec<String>>,
    },
    Where {
        input: Box<CoreTerm>,
        pred: Box<CoreTerm>,
    },
    Select {
        input: Box<CoreTerm>,
        fields: Vec<(String, CoreTerm)>,
    },
    Update {
        input: Box<CoreTerm>,
        fields: Vec<(String, CoreTerm)>,
    },
    Omit {
        input: Box<CoreTerm>,
        key: String,
    },
    Prefix {
        input: Box<CoreTerm>,
        affix: String,
    },
    Suffix {
        input: Box<CoreTerm>,
        affix: String,
    },
    Agg {
        input: Box<CoreTerm>,
        fields: Vec<(String, CoreTerm)>,
    },
    Order {
        input: Box<CoreTerm>,
        keys: Vec<(CoreTerm, bool)>,
    },
    Limit {
        input: Box<CoreTerm>,
        n: i64,
    },
    Offset {
        input: Box<CoreTerm>,
        n: i64,
    },
    Distinct {
        input: Box<CoreTerm>,
    },
    Join {
        kind: JoinKind,
        left: Box<CoreTerm>,
        right: Box<CoreTerm>,
        on: Box<CoreTerm>,
    },
    Set {
        kind: SetKind,
        left: Box<CoreTerm>,
        right: Box<CoreTerm>,
    },
    /// `Rel::At`: the stage as written at `loc`. Added only by
    /// [`CoreTerm::at`], by the caller that knows the source location.
    At {
        loc: Loc,
        input: Box<CoreTerm>,
    },

    // ── expressions ──
    Col(Side, String),
    Lit(Lit),
    Tpl {
        sql: String,
        args: Vec<CoreTerm>,
    },
    In {
        value: Box<CoreTerm>,
        list: Vec<CoreTerm>,
        negated: bool,
    },
    AggExpr {
        sql: String,
        args: Vec<CoreTerm>,
    },
    Group(Box<CoreTerm>),
    Win {
        sql: String,
        args: Vec<CoreTerm>,
        spec: CoreSpec,
    },

    // ── sort keys, frames, bounds ──
    /// A sort key: an expression and its direction. Not a standalone `Expr`;
    /// it only occurs inside `order` and a window spec's `order`.
    Dir {
        expr: Box<CoreTerm>,
        asc: bool,
    },
    Frame(Frame),
    BoundValue(Bound),
}

/// A window specification built from terms.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreSpec {
    pub partition: Vec<CoreTerm>,
    pub order: Vec<(CoreTerm, bool)>,
    pub frame: Option<Frame>,
}

impl CoreSpec {
    pub fn new(
        partition: Vec<CoreTerm>,
        order: Vec<(CoreTerm, bool)>,
        frame: Option<Frame>,
    ) -> CoreSpec {
        CoreSpec {
            partition,
            order,
            frame,
        }
    }

    /// Every partition and order key must satisfy the key rule (row phase).
    fn check(&self) -> Result<(), Error> {
        for e in &self.partition {
            key_phase(e)?;
        }
        for (e, _) in &self.order {
            key_phase(e)?;
        }
        Ok(())
    }

    /// The parts the checked layer wants, so `win` can build a
    /// `CheckedExpr` without re-deriving them.
    fn partition_terms(&self) -> Vec<CoreTerm> {
        self.partition.clone()
    }
}

// ── relations ──────────────────────────────────────────────────────────────

impl CoreTerm {
    /// `table "s" "t"`: the columns are unknown until the definition's type
    /// supplies them ([`CoreTerm::with_columns`]).
    pub fn table(schema: String, name: String) -> CoreTerm {
        CoreTerm::Table {
            schema,
            name,
            columns: None,
        }
    }

    /// Give a table its column list, from the definition's `query {..}` type.
    /// A no-op on anything but a table, so `attach_schema` can apply it to any
    /// term.
    pub fn with_columns(self, columns: Option<Vec<String>>) -> CoreTerm {
        match self {
            CoreTerm::Table { schema, name, .. } => CoreTerm::Table {
                schema,
                name,
                columns: columns.or(None).or(None),
            },
            other => other,
        }
    }

    /// The column list of a table node, if this is one and it knows them.
    ///
    /// Named `table_columns`, not `columns`: [`CoreTerm::columns`] is the list
    /// of column *references* in any term, and two methods with one name would
    /// invite a caller to take the wrong one.
    ///
    /// Like [`CoreTerm::table_columns_mut`], this descends `At`, so the two
    /// agree on what "the table in this term" means.
    pub fn table_columns(&self) -> Option<&[String]> {
        match self {
            CoreTerm::Table { columns, .. } => columns.as_deref(),
            CoreTerm::At { input, .. } => input.table_columns(),
            _ => None,
        }
    }

    /// This term without `At` wrappers at its root, so a caller can find the
    /// node underneath (the evaluator's `attach_schema` walks `Rel::At`).
    pub fn bare(&self) -> &CoreTerm {
        match self {
            CoreTerm::At { input, .. } => input.bare(),
            t => t,
        }
    }

    /// The column list of the `Table` underneath any `At` wrappers, for
    /// mutation.
    ///
    /// This is what `eval::attach_schema` needs: it descends to the table and
    /// fills in the columns the definition's `query {..}` type supplies, a
    /// step that happens *after* evaluation and only for tables. `Some` means
    /// "this term is a table, here is its column slot"; `None` means it is not
    /// a table, so there is nothing to fill.
    pub fn table_columns_mut(&mut self) -> Option<&mut Option<Vec<String>>> {
        match self {
            CoreTerm::Table { columns, .. } => Some(columns),
            CoreTerm::At { input, .. } => input.table_columns_mut(),
            _ => None,
        }
    }

    /// The stage written at `loc`. The evaluator adds this exactly where it
    /// adds `Rel::At` today, so the erased tree is unchanged.
    pub fn at(self, loc: Loc) -> CoreTerm {
        match self {
            // Wrapping an `At` in an `At` is what the evaluator already
            // avoids (`r @ Rel::At(..) => r`), so keep that here too.
            t @ CoreTerm::At { .. } => t,
            t => CoreTerm::At {
                loc,
                input: Box::new(t),
            },
        }
    }

    /// `where pred`: `pred` must be an expression of row or constant phase
    /// (the `bool` part of its type is the checker's business — a `Rel` does
    /// not carry types, and `schema` re-checks it).
    pub fn where_(input: CoreTerm, pred: CoreTerm) -> Result<CoreTerm, Error> {
        pred.phase().map_err(Error::new)?;
        Ok(CoreTerm::Where {
            input: Box::new(input),
            pred: Box::new(pred),
        })
    }

    /// `select {..}`: every field must be an expression, and the list must not
    /// be empty — the rules `schema::projection` states.
    pub fn select(input: CoreTerm, fields: Vec<(String, CoreTerm)>) -> Result<CoreTerm, Error> {
        check_fields("select", &fields)?;
        Ok(CoreTerm::Select {
            input: Box::new(input),
            fields,
        })
    }

    /// `update {..}`: the same field rules as `select`.
    pub fn update(input: CoreTerm, fields: Vec<(String, CoreTerm)>) -> Result<CoreTerm, Error> {
        check_fields("update", &fields)?;
        Ok(CoreTerm::Update {
            input: Box::new(input),
            fields,
        })
    }

    /// `agg {..}`: at least one field; each is an expression. The depth-1
    /// rule (a field is `agg`- or `const`-phase) is enforced by the phase
    /// check on the field's own term.
    pub fn agg(input: CoreTerm, fields: Vec<(String, CoreTerm)>) -> Result<CoreTerm, Error> {
        check_fields("agg", &fields)?;
        for (n, f) in &fields {
            match f.phase() {
                Ok(Phase::Agg) | Ok(Phase::Const) => {}
                Ok(p) => {
                    return Err(Error::new(format!(
                        "field `{n}` {}",
                        rules::place(rules::Place::Agg, p).unwrap_err()
                    )))
                }
                Err(m) => return Err(Error::new(format!("field `{n}`: {m}"))),
            }
        }
        Ok(CoreTerm::Agg {
            input: Box::new(input),
            fields,
        })
    }

    /// `omit "k"`.
    pub fn omit(input: CoreTerm, key: String) -> Result<CoreTerm, Error> {
        Ok(CoreTerm::Omit {
            input: Box::new(input),
            key,
        })
    }

    /// `prefix "s"`.
    pub fn prefix(input: CoreTerm, affix: String) -> Result<CoreTerm, Error> {
        Ok(CoreTerm::Prefix {
            input: Box::new(input),
            affix,
        })
    }

    /// `suffix "s"`.
    pub fn suffix(input: CoreTerm, affix: String) -> Result<CoreTerm, Error> {
        Ok(CoreTerm::Suffix {
            input: Box::new(input),
            affix,
        })
    }

    /// `order [..]`: every key must be row phase (or constant).
    pub fn order(input: CoreTerm, keys: Vec<(CoreTerm, bool)>) -> Result<CoreTerm, Error> {
        for (k, _) in &keys {
            key_phase(k)?;
        }
        Ok(CoreTerm::Order {
            input: Box::new(input),
            keys,
        })
    }

    /// `limit n`.
    pub fn limit(input: CoreTerm, n: i64) -> Result<CoreTerm, Error> {
        if n < 0 {
            return Err(Error::new(format!(
                "`limit` needs a non-negative count, got {n}"
            )));
        }
        Ok(CoreTerm::Limit {
            input: Box::new(input),
            n,
        })
    }

    /// `offset n`.
    pub fn offset(input: CoreTerm, n: i64) -> Result<CoreTerm, Error> {
        if n < 0 {
            return Err(Error::new(format!(
                "`offset` needs a non-negative count, got {n}"
            )));
        }
        Ok(CoreTerm::Offset {
            input: Box::new(input),
            n,
        })
    }

    /// `distinct`.
    pub fn distinct(input: CoreTerm) -> Result<CoreTerm, Error> {
        Ok(CoreTerm::Distinct {
            input: Box::new(input),
        })
    }

    /// A join: the predicate must be row-phase and must qualify its columns
    /// with `.<`/`.>`; a bare `.x` is rejected (`rules::needs_side`).
    pub fn join(
        kind: JoinKind,
        left: CoreTerm,
        right: CoreTerm,
        on: CoreTerm,
    ) -> Result<CoreTerm, Error> {
        match on.phase() {
            Ok(Phase::Row) | Ok(Phase::Const) => {}
            Ok(p) => {
                return Err(Error::new(
                    rules::place(rules::Place::JoinOn, p).unwrap_err(),
                ))
            }
            Err(m) => return Err(Error::new(m)),
        }
        // A plain column in a join predicate is the one mistake the side rule
        // can catch without the inputs' rows.
        if let Some(n) = on.first_plain_column() {
            return Err(Error::new(rules::needs_side(&n)));
        }
        Ok(CoreTerm::Join {
            kind,
            left: Box::new(left),
            right: Box::new(right),
            on: Box::new(on),
        })
    }

    /// A set operation. Both inputs' rows are compared by `schema`; the
    /// constructor itself has nothing local to check.
    pub fn set(kind: SetKind, left: CoreTerm, right: CoreTerm) -> CoreTerm {
        CoreTerm::Set {
            kind,
            left: Box::new(left),
            right: Box::new(right),
        }
    }
}

/// A projection/aggregate field list: non-empty, every field an expression,
/// no repeated name.
fn check_fields(stage: &str, fields: &[(String, CoreTerm)]) -> Result<(), Error> {
    if fields.is_empty() {
        return Err(Error::new(format!("`{stage}` needs at least one field")));
    }
    for (n, f) in fields {
        if matches!(f, CoreTerm::Dir { .. } | CoreTerm::Frame(_) | CoreTerm::BoundValue(_)) {
            return Err(Error::new(format!(
                "field `{n}` of `{stage}` must be a column expression or constant, found {}",
                f.kind()
            )));
        }
        f.phase().map_err(|m| Error::new(format!("field `{n}`: {m}")))?;
    }
    let mut seen: Vec<&str> = Vec::with_capacity(fields.len());
    for (n, _) in fields {
        if seen.contains(&n.as_str()) {
            return Err(Error::new(format!("`{stage}` has two fields named `{n}`")));
        }
        seen.push(n);
    }
    Ok(())
}

/// The key rule for a sort/partition key: row phase, with a `Dir` unwrapped.
fn key_phase(k: &CoreTerm) -> Result<(), Error> {
    let e = match k {
        CoreTerm::Dir { expr, .. } => expr.as_ref(),
        k => k,
    };
    match e.phase().map_err(Error::new)? {
        Phase::Row | Phase::Const => Ok(()),
        p => Err(Error::new(rules::place(rules::Place::Key, p).unwrap_err())),
    }
}

// ── expressions ────────────────────────────────────────────────────────────

impl CoreTerm {
    /// `.x`, `.<x`, `.>x`.
    pub fn col(side: Side, name: String) -> CoreTerm {
        CoreTerm::Col(side, name)
    }

    /// A literal.
    pub fn lit(l: Lit) -> CoreTerm {
        CoreTerm::Lit(l)
    }

    /// A scalar SQL template. Its phase is the mix of its arguments', so a
    /// template over row arguments is row phase and one over aggregates is an
    /// aggregate.
    pub fn tpl(sql: String, args: Vec<CoreTerm>) -> CoreTerm {
        CoreTerm::Tpl { sql, args }
    }

    /// `in`: the list and the value must mix per `rules::mix`.
    pub fn in_(
        value: CoreTerm,
        list: Vec<CoreTerm>,
        negated: bool,
    ) -> Result<CoreTerm, Error> {
        let mut phase = value.phase().map_err(Error::new)?;
        for e in &list {
            phase = rules::mix(phase, e.phase().map_err(Error::new)?).map_err(Error::new)?;
        }
        Ok(CoreTerm::In {
            value: Box::new(value),
            list,
            negated,
        })
    }

    /// An aggregate template: every argument must be row phase (depth-1).
    pub fn agg_expr(sql: String, args: Vec<CoreTerm>) -> Result<CoreTerm, Error> {
        row_args(&args)?;
        Ok(CoreTerm::AggExpr { sql, args })
    }

    /// `group key`: the key must be row phase; the result is an aggregate.
    pub fn group(key: CoreTerm) -> Result<CoreTerm, Error> {
        nested(&key, "a group key")?;
        Ok(CoreTerm::Group(Box::new(key)))
    }

    /// A window function: every argument and every window-spec key must be row
    /// phase.
    pub fn win(sql: String, args: Vec<CoreTerm>, spec: CoreSpec) -> Result<CoreTerm, Error> {
        row_args(&args)?;
        spec.check()?;
        Ok(CoreTerm::Win { sql, args, spec })
    }

    /// A sort key: an expression and its direction. `order` and a window
    /// spec's `order` accept a bare expression and treat it as ascending, the
    /// way `prims::sort_key` does.
    pub fn dir(e: CoreTerm, asc: bool) -> CoreTerm {
        CoreTerm::Dir {
            expr: Box::new(e),
            asc,
        }
    }

    /// A frame from two bounds, rejecting impossible ones.
    pub fn rows(start: Bound, end: Bound) -> Result<CoreTerm, Error> {
        if start == Bound::UnboundedFollowing
            || end == Bound::UnboundedPreceding
            || start.key() > end.key()
        {
            return Err(Error::new(format!(
                "impossible window frame: {start:?} to {end:?}"
            )));
        }
        Ok(CoreTerm::Frame(Frame { start, end }))
    }

    /// A frame bound, as a term (the evaluator keeps these in `Value::Bound`).
    pub fn bound(b: Bound) -> CoreTerm {
        CoreTerm::BoundValue(b)
    }
}

/// Every aggregate/window argument must be row phase: the depth-1 rule.
fn row_args(args: &[CoreTerm]) -> Result<(), Error> {
    for a in args {
        nested(a, "an aggregate argument")?;
    }
    Ok(())
}

/// The depth-1 rule for one argument, via `rules::nested` so the wording is
/// the one the checker and the IR validator already use.
fn nested(e: &CoreTerm, what: &str) -> Result<(), Error> {
    let p = e.phase().map_err(Error::new)?;
    rules::nested(what, p).map_err(Error::new)
}

// ── phases and kinds ───────────────────────────────────────────────────────

impl CoreTerm {
    /// A term's phase, with the same messages `ir::Expr::phase` produces.
    ///
    /// A relation, a sort key, a frame, or a bound has no phase: asking is a
    /// caller error, and it is reported rather than papered over.
    pub fn phase(&self) -> Result<Phase, String> {
        match self {
            CoreTerm::Col(..) => Ok(Phase::Row),
            CoreTerm::Lit(_) => Ok(Phase::Const),
            CoreTerm::Tpl { args, .. } => args
                .iter()
                .try_fold(Phase::Const, |acc, a| rules::mix(acc, a.phase()?)),
            CoreTerm::In { value, list, .. } => list
                .iter()
                .chain(std::iter::once(value.as_ref()))
                .try_fold(Phase::Const, |acc, a| rules::mix(acc, a.phase()?)),
            CoreTerm::AggExpr { args, .. } => {
                args.iter()
                    .try_for_each(|a| row_only(a, "an aggregate argument"))?;
                Ok(Phase::Agg)
            }
            CoreTerm::Group(k) => {
                row_only(k, "a group key")?;
                Ok(Phase::Agg)
            }
            CoreTerm::Win { args, spec, .. } => {
                args.iter()
                    .try_for_each(|a| row_only(a, "a window argument"))?;
                for e in spec.partition_terms() {
                    row_only(&e, "a window argument")?;
                }
                for (e, _) in &spec.order {
                    row_only(e, "a window argument")?;
                }
                Ok(Phase::Win)
            }
            CoreTerm::At { input, .. } => input.phase(),
            CoreTerm::Dir { expr, .. } => expr.phase(),
            other => Err(format!(
                "{} is not an expression, so it has no phase",
                other.kind()
            )),
        }
    }

    /// What a term is, for an error message.
    pub fn kind(&self) -> &'static str {
        match self {
            CoreTerm::Table { .. } => "a table",
            CoreTerm::Where { .. }
            | CoreTerm::Select { .. }
            | CoreTerm::Update { .. }
            | CoreTerm::Omit { .. }
            | CoreTerm::Prefix { .. }
            | CoreTerm::Suffix { .. }
            | CoreTerm::Agg { .. }
            | CoreTerm::Order { .. }
            | CoreTerm::Limit { .. }
            | CoreTerm::Offset { .. }
            | CoreTerm::Distinct { .. }
            | CoreTerm::Join { .. }
            | CoreTerm::Set { .. }
            | CoreTerm::At { .. } => "a query",
            CoreTerm::Col(..) | CoreTerm::Lit(_) | CoreTerm::Tpl { .. } | CoreTerm::In { .. } => {
                "a column expression"
            }
            CoreTerm::AggExpr { .. } | CoreTerm::Group(_) => "an aggregate",
            CoreTerm::Win { .. } => "a window function",
            CoreTerm::Dir { .. } => "a sort key",
            CoreTerm::Frame(_) => "a window frame",
            CoreTerm::BoundValue(_) => "a frame bound",
        }
    }

    /// The first plain (unqualified) column reference in this term, used by
    /// [`CoreTerm::join`] to catch a bare `.x` in a predicate.
    ///
    /// Owned, like [`CoreTerm::columns`] beside it: the reference lives inside
    /// this term's tree, and the previous `&str` version had to manufacture a
    /// `'static` borrow with `Box::leak`, leaking an allocation on every call
    /// (and this is called once per join predicate checked).
    pub fn first_plain_column(&self) -> Option<String> {
        let mut found = None;
        self.visit_columns(&mut |side, n| {
            if found.is_none() && matches!(side, Side::Single) {
                found = Some(n.to_string());
            }
        });
        found
    }

    /// Every column reference in this term, in reading order.
    pub fn columns(&self) -> Vec<(Side, String)> {
        let mut out = Vec::new();
        self.visit_columns(&mut |side, n| out.push((side, n.to_string())));
        out
    }

    fn visit_columns(&self, f: &mut impl FnMut(Side, &str)) {
        match self {
            CoreTerm::Col(side, n) => f(*side, n),
            CoreTerm::Table { .. }
            | CoreTerm::Lit(_)
            | CoreTerm::Frame(_)
            | CoreTerm::BoundValue(_) => {}
            CoreTerm::At { input, .. }
            | CoreTerm::Distinct { input }
            | CoreTerm::Prefix { input, .. }
            | CoreTerm::Suffix { input, .. }
            | CoreTerm::Omit { input, .. }
            | CoreTerm::Limit { input, .. }
            | CoreTerm::Offset { input, .. } => input.visit_columns(f),
            CoreTerm::Where { input, pred } => {
                input.visit_columns(f);
                pred.visit_columns(f);
            }
            CoreTerm::Select { input, fields }
            | CoreTerm::Update { input, fields }
            | CoreTerm::Agg { input, fields } => {
                input.visit_columns(f);
                for (_, e) in fields {
                    e.visit_columns(f);
                }
            }
            CoreTerm::Order { input, keys } => {
                input.visit_columns(f);
                for (k, _) in keys {
                    k.visit_columns(f);
                }
            }
            CoreTerm::Join {
                left, right, on, ..
            } => {
                left.visit_columns(f);
                right.visit_columns(f);
                on.visit_columns(f);
            }
            CoreTerm::Set { left, right, .. } => {
                left.visit_columns(f);
                right.visit_columns(f);
            }
            CoreTerm::Tpl { args, .. } | CoreTerm::AggExpr { args, .. } => {
                for a in args {
                    a.visit_columns(f);
                }
            }
            CoreTerm::In { value, list, .. } => {
                value.visit_columns(f);
                for e in list {
                    e.visit_columns(f);
                }
            }
            CoreTerm::Group(k) => k.visit_columns(f),
            CoreTerm::Win { args, spec, .. } => {
                for a in args {
                    a.visit_columns(f);
                }
                for e in &spec.partition {
                    e.visit_columns(f);
                }
                for (e, _) in &spec.order {
                    e.visit_columns(f);
                }
            }
            CoreTerm::Dir { expr, .. } => expr.visit_columns(f),
        }
    }
}

fn row_only(e: &CoreTerm, what: &str) -> Result<(), String> {
    rules::nested(what, e.phase()?)
}

// ── erasure ────────────────────────────────────────────────────────────────

/// Lower a term to the current relational IR.
///
/// Fallible, because a `CoreTerm` is an open sum: a scalar expression or a
/// window frame is a perfectly good term but not a relation. Erasing one used
/// to manufacture a placeholder table; this reports it instead. Callers that
/// hold a query use [`erase_query`], which is total.
///
/// Total and structural otherwise; the result is exactly the tree today's
/// evaluator builds for the same program, so `schema::schema` and the SQL
/// backend accept it without changes. `At` is preserved where the caller put it
/// and added nowhere else.
pub fn erase_core(t: CoreTerm) -> Result<Rel, Error> {
    // The `other` arm is the only fallible one; keeping the error out of the
    // `Ok(...)` wrapper is why this is a statement rather than one expression.
    let rel = match t {
        CoreTerm::Table {
            schema,
            name,
            columns,
        } => Rel::Table {
            schema,
            name,
            columns,
        },
        CoreTerm::Where { input, pred } => {
            Rel::Where(Box::new(erase_core(*input)?), erase_expr_core(*pred)?)
        }
        CoreTerm::Select { input, fields } => Rel::Select(
            Box::new(erase_core(*input)?),
            erase_core_fields(fields)?,
        ),
        CoreTerm::Update { input, fields } => Rel::Update(
            Box::new(erase_core(*input)?),
            erase_core_fields(fields)?,
        ),
        CoreTerm::Omit { input, key } => Rel::Omit(Box::new(erase_core(*input)?), key),
        CoreTerm::Prefix { input, affix } => Rel::Prefix(Box::new(erase_core(*input)?), affix),
        CoreTerm::Suffix { input, affix } => Rel::Suffix(Box::new(erase_core(*input)?), affix),
        CoreTerm::Agg { input, fields } => Rel::Agg(
            Box::new(erase_core(*input)?),
            erase_core_fields(fields)?,
        ),
        CoreTerm::Order { input, keys } => Rel::Order(
            Box::new(erase_core(*input)?),
            keys.into_iter()
                .map(|(k, asc)| Ok((erase_expr_core(k)?, asc)))
                .collect::<Result<Vec<_>, Error>>()?,
        ),
        CoreTerm::Limit { input, n } => Rel::Limit(Box::new(erase_core(*input)?), n),
        CoreTerm::Offset { input, n } => Rel::Offset(Box::new(erase_core(*input)?), n),
        CoreTerm::Distinct { input } => Rel::Distinct(Box::new(erase_core(*input)?)),
        CoreTerm::Join {
            kind,
            left,
            right,
            on,
        } => Rel::Join {
            kind,
            left: Box::new(erase_core(*left)?),
            right: Box::new(erase_core(*right)?),
            on: erase_expr_core(*on)?,
        },
        CoreTerm::Set { kind, left, right } => Rel::Set {
            kind,
            left: Box::new(erase_core(*left)?),
            right: Box::new(erase_core(*right)?),
        },
        CoreTerm::At { loc, input } => Rel::At(loc, Box::new(erase_core(*input)?)),
        // An expression is not a relation. This used to be erased as a
        // placeholder `Rel::Table` carrying a `/* not a query */` name, which
        // put a manufactured table into the IR and let it reach the SQL
        // backend as if it were real. A `Result` says the same thing to the
        // compiler instead of to a reader: a term that is not a query cannot
        // be erased to a relation, and the caller must handle that.
        other => {
            return Err(Error::new(format!(
                "internal error: cannot erase a non-query core term ({}) to a relation",
                other.kind()
            )))
        }
    };
    Ok(rel)
}

/// [`erase_core`] for a term that is known to be a query, which is the only
/// case the evaluator's boundary has.
///
/// The panic is reachable only if elaboration produced a non-query term where
/// a query was required — a compiler bug, not a user error — so this fails
/// loudly rather than fabricating IR. Callers that can handle the invalid case
/// should use [`erase_core`] directly.
pub fn erase_query(t: CoreTerm) -> Rel {
    match erase_core(t) {
        Ok(r) => r,
        Err(e) => panic!(
            "internal error: the evaluator produced a non-query term where a query was \
             required: {}",
            e.message
        ),
    }
}

/// Lower a term to the current `Expr`.
///
/// Fallible, for the same reason as [`erase_core`]: a `CoreTerm` is an open
/// sum, and a query or a window frame is not a scalar expression. Erasing one
/// used to manufacture a `/* not an expression */` template.
pub fn erase_expr_core(t: CoreTerm) -> Result<Expr, Error> {
    let expr = match t {
        CoreTerm::Col(side, n) => Expr::Col(side, n),
        CoreTerm::Lit(l) => Expr::Lit(l),
        CoreTerm::Tpl { sql, args } => Expr::Tpl(sql, erase_exprs_core(args)?),
        CoreTerm::In {
            value,
            list,
            negated,
        } => Expr::In(
            Box::new(erase_expr_core(*value)?),
            erase_exprs_core(list)?,
            negated,
        ),
        CoreTerm::AggExpr { sql, args } => Expr::Agg(sql, erase_exprs_core(args)?),
        CoreTerm::Group(k) => Expr::Group(Box::new(erase_expr_core(*k)?)),
        CoreTerm::Win { sql, args, spec } => Expr::Win(
            sql,
            erase_exprs_core(args)?,
            Box::new(WinSpec {
                partition: erase_exprs_core(spec.partition)?,
                order: spec
                    .order
                    .into_iter()
                    .map(|(e, asc)| Ok((erase_expr_core(e)?, asc)))
                    .collect::<Result<Vec<_>, Error>>()?,
                frame: spec.frame,
            }),
        ),
        // A sort key is an expression with a direction; as a bare expression
        // the direction is dropped, which is what `order`'s `(Expr, bool)`
        // does with the flag it carries instead.
        CoreTerm::Dir { expr, .. } => erase_expr_core(*expr)?,
        CoreTerm::At { input, .. } => erase_expr_core(*input)?,
        // A query, a frame, or a bound is not a scalar expression. This used
        // to become a `/* not an expression */` template — a *valid* SQL
        // expression, so it travelled to the backend as though it meant
        // something. `Result` says the same thing to the compiler.
        other => {
            return Err(Error::new(format!(
                "internal error: cannot erase a non-expression core term ({}) to an expression",
                other.kind()
            )))
        }
    };
    Ok(expr)
}

/// Erase a projection's fields, stopping at the first that cannot be.
fn erase_core_fields(fs: Vec<(String, CoreTerm)>) -> Result<Vec<(String, Expr)>, Error> {
    fs.into_iter()
        .map(|(n, e)| Ok((n, erase_expr_core(e)?)))
        .collect()
}

/// Erase a list of core expressions, stopping at the first that cannot be.
fn erase_exprs_core(ts: Vec<CoreTerm>) -> Result<Vec<Expr>, Error> {
    ts.into_iter().map(erase_expr_core).collect()
}

/// [`erase_expr_core`] for a term known to be an expression.
///
/// The panic is reachable only if a caller hands over a query where an
/// expression belongs, which is a compiler bug; failing loudly is better than
/// the placeholder template this used to emit. Callers that can handle the
/// invalid case use [`erase_expr_core`] directly.
pub fn erase_expr_query(t: CoreTerm) -> Expr {
    match erase_expr_core(t) {
        Ok(e) => e,
        Err(e) => panic!("internal error: {}", e.message),
    }
}

/// Erase, then let `schema::schema` re-derive the columns.
///
/// The constructors have already checked what this validator re-derives, so
/// the comparison is the test that ties the two layers together.
pub fn erase_core_checked(t: CoreTerm) -> Result<(Rel, Vec<String>), String> {
    let rel = erase_core(t).map_err(|e| e.message)?;
    let cols = crate::schema::schema(&rel)?;
    Ok((rel, cols))
}

// ── the bridge from a checked query ────────────────────────────────────────

/// The `CoreTerm` a checked query erases to, as a term rather than a `Rel`.
///
/// This is what makes `CheckedQuery::erase()` and `CoreTerm::erase()` agree:
/// both go through the same shape, so the checked tree and the evaluated tree
/// cannot drift.
///
/// Fallible only through [`of_checked_expr`]: a query node always has a term,
/// but an expression inside one may be a `Call`, which has none. No *query*
/// node can fail, which the `Result` makes visible rather than assumed.
pub fn of_checked(q: CheckedQuery) -> Result<CoreTerm, Error> {
    let (_row, node, origin) = q.into_parts();
    let t = match node {
        checked::CheckedQueryNode::Table {
            schema,
            name,
            columns,
        } => CoreTerm::Table {
            schema,
            name,
            columns: Some(columns),
        },
        checked::CheckedQueryNode::Where { input, pred } => CoreTerm::Where {
            input: Box::new(of_checked(*input)?),
            pred: Box::new(of_checked_expr(pred)?),
        },
        checked::CheckedQueryNode::Select { input, fields } => CoreTerm::Select {
            input: Box::new(of_checked(*input)?),
            fields: fields
                .into_iter()
                .map(|(n, e)| Ok((n, of_checked_expr(e)?)))
                .collect::<Result<Vec<_>, Error>>()?,
        },
        checked::CheckedQueryNode::Update { input, fields } => CoreTerm::Update {
            input: Box::new(of_checked(*input)?),
            fields: fields
                .into_iter()
                .map(|(n, e)| Ok((n, of_checked_expr(e)?)))
                .collect::<Result<Vec<_>, Error>>()?,
        },
        checked::CheckedQueryNode::Omit { input, key } => CoreTerm::Omit {
            input: Box::new(of_checked(*input)?),
            key,
        },
        checked::CheckedQueryNode::Rename {
            input,
            affix,
            prefix,
        } => {
            let inner = Box::new(of_checked(*input)?);
            if prefix {
                CoreTerm::Prefix {
                    input: inner,
                    affix,
                }
            } else {
                CoreTerm::Suffix {
                    input: inner,
                    affix,
                }
            }
        }
        checked::CheckedQueryNode::Agg { input, fields } => CoreTerm::Agg {
            input: Box::new(of_checked(*input)?),
            fields: fields
                .into_iter()
                .map(|(n, e)| Ok((n, of_checked_expr(e)?)))
                .collect::<Result<Vec<_>, Error>>()?,
        },
        checked::CheckedQueryNode::Order { input, keys } => CoreTerm::Order {
            input: Box::new(of_checked(*input)?),
            keys: keys
                .into_iter()
                .map(|(e, asc)| Ok((of_checked_expr(e)?, asc)))
                .collect::<Result<Vec<_>, Error>>()?,
        },
        checked::CheckedQueryNode::Limit { input, n } => CoreTerm::Limit {
            input: Box::new(of_checked(*input)?),
            n,
        },
        checked::CheckedQueryNode::Offset { input, n } => CoreTerm::Offset {
            input: Box::new(of_checked(*input)?),
            n,
        },
        checked::CheckedQueryNode::Distinct(input) => CoreTerm::Distinct {
            input: Box::new(of_checked(*input)?),
        },
        checked::CheckedQueryNode::Join {
            kind,
            left,
            right,
            on,
        } => CoreTerm::Join {
            kind,
            left: Box::new(of_checked(*left)?),
            right: Box::new(of_checked(*right)?),
            on: Box::new(of_checked_expr(on)?),
        },
        checked::CheckedQueryNode::Set { kind, left, right } => CoreTerm::Set {
            kind,
            left: Box::new(of_checked(*left)?),
            right: Box::new(of_checked(*right)?),
        },
    };
    // A checked query carries an `Origin`, not an `At`; the location is
    // stamped here so the erased tree matches the evaluator's, which adds
    // `Rel::At` at its application sites.
    Ok(t.at(Loc {
        module: origin.module,
        span: origin.span,
    }))
}

/// The `CoreTerm` a checked expression erases to.
///
/// Fallible for one reason: a `CheckedExprNode::Call` is not a term this layer
/// can express, because a call must be inlined or reduced before erasure. It
/// used to become a `/* unresolved call */` template — valid SQL, so it reached
/// the backend as though it were a value. `Err` says the same thing to the
/// compiler instead.
pub fn of_checked_expr(e: CheckedExpr) -> Result<CoreTerm, Error> {
    let (_phase, _ty, node, _origin) = e.into_parts();
    let t = match node {
        checked::CheckedExprNode::Column { side, name } => CoreTerm::Col(side, name),
        checked::CheckedExprNode::Lit(l) => CoreTerm::Lit(l),
        checked::CheckedExprNode::Template { sql, args } => CoreTerm::Tpl {
            sql,
            args: of_checked_exprs(args)?,
        },
        checked::CheckedExprNode::AggTemplate { sql, args } => CoreTerm::AggExpr {
            sql,
            args: of_checked_exprs(args)?,
        },
        checked::CheckedExprNode::WinTemplate { sql, args, spec } => CoreTerm::Win {
            sql,
            args: of_checked_exprs(args)?,
            spec: of_checked_spec(spec)?,
        },
        checked::CheckedExprNode::In {
            value,
            list,
            negated,
            ..
        } => CoreTerm::In {
            value: Box::new(of_checked_expr(*value)?),
            list: of_checked_exprs(list)?,
            negated,
        },
        checked::CheckedExprNode::Group(k) => CoreTerm::Group(Box::new(of_checked_expr(*k)?)),
        checked::CheckedExprNode::Call { name, .. } => {
            return Err(Error::new(format!(
                "internal error: the call `{name}` reached erasure; every call must be \
                 inlined or reduced before a checked query is erased"
            )))
        }
    };
    Ok(t)
}

/// Erase a list of checked expressions to core terms, stopping at the first
/// that cannot be.
fn of_checked_exprs(es: Vec<CheckedExpr>) -> Result<Vec<CoreTerm>, Error> {
    es.into_iter().map(of_checked_expr).collect()
}

/// The `CoreSpec` a checked window spec erases to.
pub fn of_checked_spec(spec: WinSpecChecked) -> Result<CoreSpec, Error> {
    Ok(CoreSpec {
        partition: of_checked_exprs(spec.partition)?,
        order: spec
            .order
            .into_iter()
            .map(|(e, asc)| Ok((of_checked_expr(e)?, asc)))
            .collect::<Result<Vec<_>, Error>>()?,
        frame: spec.frame,
    })
}

/// The span an `At` node carries, if it is one.
pub fn loc_of(t: &CoreTerm) -> Option<Loc> {
    match t {
        CoreTerm::At { loc, .. } => Some(*loc),
        _ => None,
    }
}

/// A location for a term built outside evaluation (tests, and the checked
/// layer's own spans).
pub fn loc(module: usize, span: Span) -> Loc {
    Loc { module, span }
}

/// The untyped twin of a `Rel`, for a caller that has IR and wants a term.
pub fn of_rel(r: Rel) -> CoreTerm {
    match r {
        Rel::Table {
            schema,
            name,
            columns,
        } => CoreTerm::Table {
            schema,
            name,
            columns,
        },
        Rel::Where(input, e) => CoreTerm::Where {
            input: Box::new(of_rel(*input)),
            pred: Box::new(of_expr(e)),
        },
        Rel::Select(input, fs) => CoreTerm::Select {
            input: Box::new(of_rel(*input)),
            fields: fs.into_iter().map(|(n, e)| (n, of_expr(e))).collect(),
        },
        Rel::Update(input, fs) => CoreTerm::Update {
            input: Box::new(of_rel(*input)),
            fields: fs.into_iter().map(|(n, e)| (n, of_expr(e))).collect(),
        },
        Rel::Omit(input, key) => CoreTerm::Omit {
            input: Box::new(of_rel(*input)),
            key,
        },
        Rel::Prefix(input, affix) => CoreTerm::Prefix {
            input: Box::new(of_rel(*input)),
            affix,
        },
        Rel::Suffix(input, affix) => CoreTerm::Suffix {
            input: Box::new(of_rel(*input)),
            affix,
        },
        Rel::Agg(input, fs) => CoreTerm::Agg {
            input: Box::new(of_rel(*input)),
            fields: fs.into_iter().map(|(n, e)| (n, of_expr(e))).collect(),
        },
        Rel::Order(input, keys) => CoreTerm::Order {
            input: Box::new(of_rel(*input)),
            keys: keys
                .into_iter()
                .map(|(e, asc)| (of_expr(e), asc))
                .collect(),
        },
        Rel::Limit(input, n) => CoreTerm::Limit {
            input: Box::new(of_rel(*input)),
            n,
        },
        Rel::Offset(input, n) => CoreTerm::Offset {
            input: Box::new(of_rel(*input)),
            n,
        },
        Rel::Distinct(input) => CoreTerm::Distinct {
            input: Box::new(of_rel(*input)),
        },
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => CoreTerm::Join {
            kind,
            left: Box::new(of_rel(*left)),
            right: Box::new(of_rel(*right)),
            on: Box::new(of_expr(on)),
        },
        Rel::Set { kind, left, right } => CoreTerm::Set {
            kind,
            left: Box::new(of_rel(*left)),
            right: Box::new(of_rel(*right)),
        },
        Rel::At(l, input) => CoreTerm::At {
            loc: l,
            input: Box::new(of_rel(*input)),
        },
    }
}

/// The untyped twin of an `Expr`.
pub fn of_expr(e: Expr) -> CoreTerm {
    match e {
        Expr::Col(side, n) => CoreTerm::Col(side, n),
        Expr::Lit(l) => CoreTerm::Lit(l),
        Expr::Tpl(sql, args) => CoreTerm::Tpl {
            sql,
            args: args.into_iter().map(of_expr).collect(),
        },
        Expr::In(value, list, negated) => CoreTerm::In {
            value: Box::new(of_expr(*value)),
            list: list.into_iter().map(of_expr).collect(),
            negated,
        },
        Expr::Agg(sql, args) => CoreTerm::AggExpr {
            sql,
            args: args.into_iter().map(of_expr).collect(),
        },
        Expr::Group(k) => CoreTerm::Group(Box::new(of_expr(*k))),
        Expr::Win(sql, args, spec) => CoreTerm::Win {
            sql,
            args: args.into_iter().map(of_expr).collect(),
            spec: CoreSpec {
                partition: spec.partition.into_iter().map(of_expr).collect(),
                order: spec
                    .order
                    .into_iter()
                    .map(|(e, asc)| (of_expr(e), asc))
                    .collect(),
                frame: spec.frame,
            },
        },
    }
}

/// A term's `At`-free depth-first walk, for a traversal that does not want to
/// know the variant list.
pub fn children(t: &CoreTerm) -> Vec<&CoreTerm> {
    match t {
        CoreTerm::Table { .. }
        | CoreTerm::Col(..)
        | CoreTerm::Lit(_)
        | CoreTerm::Frame(_)
        | CoreTerm::BoundValue(_) => vec![],
        CoreTerm::Where { input, pred } => vec![input, pred],
        CoreTerm::Select { input, fields }
        | CoreTerm::Update { input, fields }
        | CoreTerm::Agg { input, fields } => {
            let mut out: Vec<&CoreTerm> = vec![input];
            out.extend(fields.iter().map(|(_, e)| e));
            out
        }
        CoreTerm::Omit { input, .. }
        | CoreTerm::Prefix { input, .. }
        | CoreTerm::Suffix { input, .. }
        | CoreTerm::Limit { input, .. }
        | CoreTerm::Offset { input, .. }
        | CoreTerm::Distinct { input }
        | CoreTerm::At { input, .. } => vec![input],
        CoreTerm::Order { input, keys } => {
            let mut out: Vec<&CoreTerm> = vec![input];
            out.extend(keys.iter().map(|(k, _)| k));
            out
        }
        CoreTerm::Join {
            left, right, on, ..
        } => vec![left, right, on],
        CoreTerm::Set { left, right, .. } => vec![left, right],
        CoreTerm::Tpl { args, .. } | CoreTerm::AggExpr { args, .. } => args.iter().collect(),
        CoreTerm::In { value, list, .. } => {
            list.iter().chain(std::iter::once(value.as_ref())).collect()
        }
        CoreTerm::Group(k) => vec![k],
        CoreTerm::Win { args, spec, .. } => {
            let mut out: Vec<&CoreTerm> = args.iter().collect();
            out.extend(spec.partition.iter());
            out.extend(spec.order.iter().map(|(e, _)| e));
            out
        }
        CoreTerm::Dir { expr, .. } => vec![expr],
    }
}

/// The `ir` names a caller building a term needs, re-exported so a stage
/// constructor's call site does not have to import `ir` as well.
pub use ir::{Bound as FrameBoundRef, JoinKind as JoinKindRef};

#[cfg(test)]
mod tests;
