//! Checked expressions and shared validation helpers for normalization.
//!
//! This is the checked expression layer used by normalization. It keeps phase,
//! type, and expression shape together so production normalization does not
//! have to reconstruct those facts.
//!
//! The layer is deliberately small:
//!
//!   * every node carries its [`Origin`], so an error names the expression
//!     that created the invalid operation without changing the tree's shape;
//!   * every constructor checks its local rule against [`rules`].
//!
//! Phase and join-side rules come from `crate::rules`, the same module the
//! checker uses. Column-level name rules come from `crate::schema`.

use crate::check::Choice;
use crate::core::{Error, Origin, RowType, ScalarType};
use crate::ir::{Bound, Expr, Frame, JoinKind, Lit, Phase, WinSpec};
use crate::rules::{self, Place};
use cagara_syntax::ast::Side;

/// Which expression form produced a [`CheckedExpr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExprKind {
    /// A column reference (`.x`, `.<x`, `.>x`).
    Column,
    /// A scalar literal.
    Lit,
    /// A scalar SQL template (`sql "..."`).
    Template,
    /// An aggregate template (`agg`).
    AggTemplate,
    /// A window template (`win`).
    WinTemplate,
    /// `in`.
    In,
    /// `group`.
    Group,
    /// A call still to be inlined by elaboration.
    Call,
}

/// The output row of a join, per kind. `join_columns` (via `RowType::merge`)
/// is the same left-wins rule `schema` uses, and the nullable side's types go
/// through `map_value_nullable`, the `mapValue (AsNullable)` term the checker
/// builds.
pub(crate) fn join_row(kind: JoinKind, left: &RowType, right: &RowType) -> RowType {
    match kind {
        JoinKind::Semi | JoinKind::Anti => left.clone(),
        JoinKind::Inner => left.merge(right),
        JoinKind::Left => left.merge(&right.map_value_nullable()),
        JoinKind::Right => left.map_value_nullable().merge(right),
        JoinKind::Full => left.map_value_nullable().merge(&right.map_value_nullable()),
    }
}

// ── the checked expression ─────────────────────────────────────────────────

/// A checked expression: its phase, its scalar type, its node, its origin.
///
/// Private fields keep the phase, scalar type, and node in agreement — an
/// `agg`-phase node carrying a
/// `row` phase would place an aggregate where SQL rejects one — and the
/// constructors are what establish that agreement. Use [`CheckedExpr::phase`],
/// [`CheckedExpr::ty`], [`CheckedExpr::node`] and [`CheckedExpr::origin`].
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedExpr {
    phase: Phase,
    ty: ScalarType,
    node: CheckedExprNode,
    origin: Origin,
}

impl CheckedExpr {
    /// The phase this expression sits in (`const`, `row`, `agg`, `win`).
    ///
    /// Public because it is part of what a consumer needs to know about an
    /// expression; the *node* stays private, so the phase cannot be paired
    /// with a node that contradicts it.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The scalar type this expression has, for a consumer that needs it
    /// without the node.
    pub fn ty(&self) -> &ScalarType {
        &self.ty
    }

    /// Where the expression was written.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Consume this expression, yielding its parts; crate-private, as with
    /// the checked node.
    pub(crate) fn into_parts(self) -> (Phase, ScalarType, CheckedExprNode, Origin) {
        (self.phase, self.ty, self.node, self.origin)
    }

    /// Which expression form built this, without exposing the node type.
    pub fn kind(&self) -> ExprKind {
        match &self.node {
            CheckedExprNode::Column { .. } => ExprKind::Column,
            CheckedExprNode::Lit(_) => ExprKind::Lit,
            CheckedExprNode::Template { .. } => ExprKind::Template,
            CheckedExprNode::AggTemplate { .. } => ExprKind::AggTemplate,
            CheckedExprNode::WinTemplate { .. } => ExprKind::WinTemplate,
            CheckedExprNode::In { .. } => ExprKind::In,
            CheckedExprNode::Group(_) => ExprKind::Group,
            CheckedExprNode::Call { .. } => ExprKind::Call,
        }
    }

    /// Number of checked expression nodes owned by this expression.
    ///
    /// A call-by-name binding copies its checked tree each time it is forced;
    /// the normalizer uses this count to charge that expansion to its budget.
    pub(crate) fn node_count(&self) -> usize {
        1 + match &self.node {
            CheckedExprNode::Column { .. }
            | CheckedExprNode::Lit(_)
            | CheckedExprNode::Call { .. } => 0,
            CheckedExprNode::Template { args, .. } | CheckedExprNode::AggTemplate { args, .. } => {
                args.iter().map(CheckedExpr::node_count).sum()
            }
            CheckedExprNode::WinTemplate { args, spec, .. } => {
                args.iter().map(CheckedExpr::node_count).sum::<usize>()
                    + spec
                        .partition
                        .iter()
                        .map(CheckedExpr::node_count)
                        .sum::<usize>()
                    + spec
                        .order
                        .iter()
                        .map(|(expr, _)| expr.node_count())
                        .sum::<usize>()
            }
            CheckedExprNode::In { value, list, .. } => {
                value.node_count() + list.iter().map(CheckedExpr::node_count).sum::<usize>()
            }
            CheckedExprNode::Group(key) => key.node_count(),
        }
    }
    /// A column reference (`.x`, `.<x`, `.>x`). Row phase; the type is the one
    /// the checker gave the column.
    pub fn column(side: Side, name: impl Into<String>, ty: ScalarType, origin: Origin) -> Self {
        CheckedExpr {
            phase: Phase::Row,
            ty,
            node: CheckedExprNode::Column {
                side,
                name: name.into(),
            },
            origin,
        }
    }

    /// A literal. `Const` phase, with the literal's own type.
    pub fn lit(lit: Lit, origin: Origin) -> Self {
        let ty = match &lit {
            Lit::Int(_) => ScalarType::Int,
            Lit::Float(_) => ScalarType::Float,
            Lit::Str(_) => ScalarType::String,
            Lit::Bool(_) => ScalarType::Bool,
        };
        CheckedExpr {
            phase: Phase::Const,
            ty,
            node: CheckedExprNode::Lit(lit),
            origin,
        }
    }

    /// A literal written inside an expression of phase `phase`.
    ///
    /// A constant takes the phase of the expression it is part of — that is
    /// what [`rules::mix`] says about `const` — and the checker decides which
    /// expression that is when it types the literal. A caller that builds the
    /// literal before the expression has to say the same thing here.
    pub fn const_at(lit: Lit, phase: Phase, origin: Origin) -> Self {
        CheckedExpr {
            phase,
            ..Self::lit(lit, origin)
        }
    }

    /// A scalar SQL template: its phase is the [`rules::mix`] of its
    /// arguments', so a template over row arguments is row phase and one over
    /// aggregate arguments is an aggregate.
    pub fn template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        let phase = mix_of(&args, origin)?;
        Ok(CheckedExpr {
            phase,
            ty,
            node: CheckedExprNode::Template {
                sql: sql.into(),
                args,
            },
            origin,
        })
    }

    /// An aggregate template: every argument must be row phase (the depth-1
    /// rule — aggregates do not nest). The result is `Agg` phase whatever the
    /// arguments' own phases were, as long as they were row/const.
    pub fn agg_template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        row_args("an aggregate argument", &args, origin)?;
        Ok(CheckedExpr {
            phase: Phase::Agg,
            ty,
            node: CheckedExprNode::AggTemplate {
                sql: sql.into(),
                args,
            },
            origin,
        })
    }

    /// A window template: every argument and every window-spec expression must
    /// be row phase.
    pub fn win_template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        spec: WinSpecChecked,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        row_args("a window argument", &args, origin)?;
        spec.check()?;
        Ok(CheckedExpr {
            phase: Phase::Win,
            ty,
            node: CheckedExprNode::WinTemplate {
                sql: sql.into(),
                args,
                spec,
            },
            origin,
        })
    }

    /// `group e`: the key must be row phase; the result is an aggregate.
    pub fn group(key: CheckedExpr, origin: Origin) -> Result<Self, Error> {
        row_phase("a group key", &key, origin)?;
        Ok(CheckedExpr {
            phase: Phase::Agg,
            ty: key.ty.clone(),
            node: CheckedExprNode::Group(Box::new(key)),
            origin,
        })
    }

    /// `in`: value and list mix per [`rules::mix`]; the result is bool at the
    /// mixed phase.
    pub fn in_(
        value: CheckedExpr,
        list: Vec<CheckedExpr>,
        negated: bool,
        origin: Origin,
    ) -> Result<Self, Error> {
        let mut phase = value.phase;
        for e in &list {
            phase = rules::mix(phase, e.phase).map_err(|m| Error::new(m).at(e.origin))?;
        }
        let elem = value.ty.clone();
        Ok(CheckedExpr {
            phase,
            ty: ScalarType::Bool,
            node: CheckedExprNode::In {
                value: Box::new(value),
                list,
                negated,
                elem,
            },
            origin,
        })
    }

    /// Every column this expression reads, in reading order.
    ///
    /// The join constructor appends them (a join predicate's names are
    /// side-qualified) and the column-reference rule tests them against the
    /// input row, so both readers ask the node rather than walking it again.
    pub(crate) fn columns(&self, out: &mut Vec<(Side, String)>) {
        self.node.collect_columns(out);
    }

    /// A use of a checked definition, with the instantiation chosen for its
    /// overload holes. This is what makes an overload choice explicit at the
    /// call site instead of a dynamic lookup during normalization.
    pub fn call(
        name: impl Into<String>,
        def: (usize, usize),
        holes: Vec<Choice>,
        ty: ScalarType,
        origin: Origin,
    ) -> Self {
        CheckedExpr {
            phase: Phase::Row,
            ty,
            node: CheckedExprNode::Call {
                name: name.into(),
                def,
                holes,
            },
            origin,
        }
    }
}

/// A checked expression's node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckedExprNode {
    Column {
        side: Side,
        name: String,
    },
    Lit(Lit),
    Template {
        sql: String,
        args: Vec<CheckedExpr>,
    },
    AggTemplate {
        sql: String,
        args: Vec<CheckedExpr>,
    },
    WinTemplate {
        sql: String,
        args: Vec<CheckedExpr>,
        spec: WinSpecChecked,
    },
    In {
        value: Box<CheckedExpr>,
        list: Vec<CheckedExpr>,
        negated: bool,
        /// The value's type, resolved once here so `erase` does not look at
        /// the elements again.
        elem: ScalarType,
    },
    Group(Box<CheckedExpr>),
    /// A checked definition used as an expression: the callee's name and the
    /// instantiation chosen for its holes.
    Call {
        name: String,
        def: (usize, usize),
        holes: Vec<Choice>,
    },
}

impl CheckedExprNode {
    /// Every column this expression reads, in reading order.
    ///
    /// Production: the join constructor and the column-reference check both
    /// need it to test a predicate's names against the input rows.
    pub(crate) fn columns(&self) -> Vec<(Side, String)> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut Vec<(Side, String)>) {
        match self {
            CheckedExprNode::Column { side, name } => out.push((*side, name.clone())),
            CheckedExprNode::Lit(_) | CheckedExprNode::Call { .. } => {}
            CheckedExprNode::Template { args, .. } | CheckedExprNode::AggTemplate { args, .. } => {
                for a in args {
                    a.node.collect_columns(out);
                }
            }
            CheckedExprNode::WinTemplate { args, spec, .. } => {
                for a in args {
                    a.node.collect_columns(out);
                }
                spec.collect_columns(out);
            }
            CheckedExprNode::In { value, list, .. } => {
                for a in list.iter().chain(std::iter::once(value.as_ref())) {
                    a.node.collect_columns(out);
                }
            }
            CheckedExprNode::Group(k) => k.node.collect_columns(out),
        }
    }
}

/// A checked window specification.
#[derive(Debug, Clone, PartialEq)]
pub struct WinSpecChecked {
    pub partition: Vec<CheckedExpr>,
    pub order: Vec<(CheckedExpr, bool)>,
    pub frame: Option<Frame>,
}

impl WinSpecChecked {
    /// Every partition and order expression must satisfy the key rule.
    pub fn check(&self) -> Result<(), Error> {
        for e in self.partition.iter() {
            place(Place::Key, e, e.origin)?;
        }
        for (e, _) in &self.order {
            place(Place::Key, e, e.origin)?;
        }
        Ok(())
    }

    fn collect_columns(&self, out: &mut Vec<(Side, String)>) {
        for e in self.partition.iter() {
            e.node.collect_columns(out);
        }
        for (e, _) in &self.order {
            e.node.collect_columns(out);
        }
    }
}

/// A window spec from its parts, checking the frame.
pub fn window_spec(
    partition: Vec<CheckedExpr>,
    order: Vec<(CheckedExpr, bool)>,
    frame: Option<Frame>,
) -> Result<WinSpecChecked, Error> {
    let spec = WinSpecChecked {
        partition,
        order,
        frame,
    };
    spec.check()?;
    Ok(spec)
}

/// A `ROWS BETWEEN` frame from two bounds, rejecting impossible ones (the
/// check for `__rows`).
pub fn frame(start: Bound, end: Bound) -> Result<Frame, Error> {
    if start == Bound::UnboundedFollowing
        || end == Bound::UnboundedPreceding
        || start.key() > end.key()
    {
        return Err(Error::new(format!(
            "impossible window frame: {start:?} to {end:?}"
        )));
    }
    Ok(Frame { start, end })
}

// ── the shared rules ───────────────────────────────────────────────────────

/// A node's phase from its arguments, via the existing mix rule.
fn mix_of(args: &[CheckedExpr], origin: Origin) -> Result<Phase, Error> {
    args.iter().try_fold(Phase::Const, |acc, a| {
        rules::mix(acc, a.phase)
            .map_err(|m| Error::new(m).at(a.origin))
            .map_err(|e| e.at(origin))
    })
}

/// The existing placement rule, pointed at the offending expression.
pub(crate) fn place(place: Place, e: &CheckedExpr, origin: Origin) -> Result<(), Error> {
    rules::place(place, e.phase)
        .map_err(|m| Error::new(m).at(e.origin))
        .map_err(|err| err.at(origin))
}

/// The existing depth-1 rule, pointed at the offending expression.
fn row_phase(what: &str, e: &CheckedExpr, origin: Origin) -> Result<(), Error> {
    rules::nested(what, e.phase)
        .map_err(|m| Error::new(m).at(e.origin))
        .map_err(|err| err.at(origin))
}

fn row_args(what: &str, args: &[CheckedExpr], origin: Origin) -> Result<(), Error> {
    for a in args {
        row_phase(what, a, origin)?;
    }
    Ok(())
}

/// Every plain column reference must be a column of `input`; a side-qualified
/// one is a join-predicate form and is rejected outside a join
/// (`rules::JOIN_ONLY`, the same message `schema::refs` gives).
pub(crate) fn refs_columns(
    e: &CheckedExpr,
    input: &RowType,
    ctx: &str,
    origin: Origin,
) -> Result<(), Error> {
    for (side, n) in e.node.columns() {
        match side {
            Side::Single if !input.has(&n) => {
                let avail = input.names().join(", ");
                return Err(Error::new(format!(
                    "no column `{n}` in the input of `{ctx}`; available: {avail}"
                ))
                .at(origin));
            }
            Side::Left | Side::Right => return Err(Error::new(rules::JOIN_ONLY).at(origin)),
            _ => {}
        }
    }
    Ok(())
}

/// The output row of `select`/`update`/`agg`: at least one field, each field
/// placed by `at`, each referring to `input`, labelled with its own type.
///
/// `at` is `Place::Select` for `select`/`update` and `Place::Agg` for `agg`;
/// the message for a rejected field is the one the checker's `stage_phase`
/// produces, so a user sees one explanation.
pub(crate) fn project_row(
    stage: &str,
    input: &RowType,
    fields: &[(String, CheckedExpr)],
    at: Place,
    origin: Origin,
) -> Result<RowType, Error> {
    if fields.is_empty() {
        return Err(Error::new(format!("`{stage}` needs at least one field")).at(origin));
    }
    // A repeated field is reported before anything else, with the wording the
    // checker uses per stage (`check/infer.rs` `Cons::Update`: "field `n`
    // appears twice in `update`"; `Cons::Project` runs the same rule for
    // `select`/`agg`), so a user sees one explanation rather than two.
    for (i, (n, _)) in fields.iter().enumerate() {
        if fields[..i].iter().any(|(o, _)| o == n) {
            return Err(Error::new(format!("field `{n}` appears twice in `{stage}`")).at(origin));
        }
    }
    let mut out = Vec::with_capacity(fields.len());
    for (n, e) in fields {
        refs_columns(e, input, stage, origin)
            .map_err(|err| Error::new(format!("field `{n}`: {}", err.message)).at(e.origin))?;
        rules::place(at, e.phase)
            .map_err(|m| Error::new(format!("field `{n}` {m}")).at(e.origin))?;
        out.push((n.clone(), e.ty.clone()));
    }
    Ok(RowType::project(&out))
}

/// `update`'s fields are placed like `select`'s: row phase, over the input.
pub(crate) fn update_place() -> Place {
    Place::Select
}

// ── erasure to the existing IR ─────────────────────────────────────────────

/// Lower a checked expression to the current `Expr`.
///
/// Fallible because a
/// `CheckedExprNode::Call` is not a SQL expression, and erasing it used to
/// emit a `/* unresolved call */` template — a *valid* SQL expression that
/// therefore reached the backend as though it meant something. An unresolved
/// call is a compiler bug; a `Result` is how the caller finds out.
pub(crate) fn erase_expr(e: CheckedExpr) -> Result<Expr, Error> {
    let (_phase, _ty, node, _origin) = e.into_parts();
    let expr = match node {
        CheckedExprNode::Column { side, name } => Expr::Col(side, name),
        CheckedExprNode::Lit(l) => Expr::Lit(l),
        CheckedExprNode::Template { sql, args } => Expr::Tpl(sql, erase_exprs(args)?),
        CheckedExprNode::AggTemplate { sql, args } => Expr::Agg(sql, erase_exprs(args)?),
        CheckedExprNode::WinTemplate { sql, args, spec } => Expr::Win(
            sql,
            erase_exprs(args)?,
            Box::new(WinSpec {
                partition: erase_exprs(spec.partition)?,
                order: spec
                    .order
                    .into_iter()
                    .map(|(e, asc)| Ok((erase_expr(e)?, asc)))
                    .collect::<Result<Vec<_>, Error>>()?,
                frame: spec.frame,
            }),
        ),
        CheckedExprNode::In {
            value,
            list,
            negated,
            ..
        } => Expr::In(Box::new(erase_expr(*value)?), erase_exprs(list)?, negated),
        CheckedExprNode::Group(k) => Expr::Group(Box::new(erase_expr(*k)?)),
        CheckedExprNode::Call { name, .. } => {
            return Err(Error::new(format!(
                "internal error: the call `{name}` reached erasure; every call must be \
                 inlined or reduced before the expression is erased"
            )))
        }
    };
    Ok(expr)
}

/// Erase a list of checked expressions, stopping at the first that cannot be.
fn erase_exprs(es: Vec<CheckedExpr>) -> Result<Vec<Expr>, Error> {
    es.into_iter().map(erase_expr).collect()
}
