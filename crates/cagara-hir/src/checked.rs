//! The checked layer: typed relational construction.
//!
//! This is the target design's "one place for relational validity". Today a
//! stage is constrained by row equations in `check/`, checked constructors,
//! and schema validation. Here the
//! constructor *is* the rule: a [`CheckedQuery`] that exists has an output row
//! that matches its node, so no later phase has to re-derive anything.
//!
//! The layer is deliberately boring:
//!
//!   * every node carries its [`Origin`], so an error names the node that
//!     created the invalid operation without changing the tree's shape;
//!   * every constructor is `pub fn … -> Result<…, Error>`, checks its local
//!     rule against [`rules`], and returns the output row;
//!   * [`erase`] lowers a tree back to the existing [`Rel`], so the SQL
//!     backend and `schema::schema` keep working unchanged while the
//!     constructors take over the checking.
//!
//! Phase and join-side rules come from `crate::rules` — the module the
//! checker's *types* already use — so this is one statement of the rule, not a
//! third one. Column-level name rules come from `crate::schema`.

use crate::check::Choice;
use crate::core::{Error, Origin, RowType, ScalarType};
use crate::ir::{Bound, Expr, Frame, JoinKind, Lit, Loc, Phase, Rel, SetKind, WinSpec};
use crate::rules::{self, Place};
use cagara_syntax::ast::Side;
use std::sync::Arc;

// ── the checked query ──────────────────────────────────────────────────────

/// Which stage produced a [`CheckedQuery`].
///
/// The node enum itself is crate-private, so this is how a consumer outside
/// the crate branches on a query's shape without being able to build one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryKind {
    Table,
    Where,
    Select,
    Update,
    Omit,
    /// `prefix` or `suffix`.
    Rename,
    Agg,
    Order,
    Limit,
    Offset,
    Distinct,
    Join,
    Set,
}

/// Which expression form produced a [`CheckedExpr`]; see [`QueryKind`].
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

/// A checked query: an output row, a node that produced it, and where the
/// stage was written.
///
/// The fields are private, and no public constructor takes a `row` and a
/// `node` separately. That is the point of the type: each constructor below
/// *checks* its stage's rule before assembling these three fields, so a
/// `CheckedQuery` that exists has a `row` its `node` actually produces. A
/// public field would let a caller pair any row with any node and silently
/// falsify every downstream guarantee, including the `erase`/`schema`
/// invariant the tests assert.
///
/// Read access is through [`CheckedQuery::row`], [`CheckedQuery::node`] and
/// [`CheckedQuery::origin`]. `node` returns the crate-private
/// `CheckedQueryNode`, so a consumer outside this crate can inspect a shape
/// but cannot construct one — which is what makes "the constructors are the
/// only way" a fact rather than a convention.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedQuery {
    row: Arc<RowType>,
    node: CheckedQueryNode,
    origin: Origin,
}

impl CheckedQuery {
    /// The row this query produces: ordered `(column, type)` pairs.
    pub fn row(&self) -> &RowType {
        &self.row
    }

    /// Where the stage was written.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Consume this query, yielding its parts.
    ///
    /// Crate-private: only the erasure path
    /// `erase`) needs to take a `CheckedQuery` apart by value, and it lives in
    /// this crate. Keeping it `pub(crate)` means the *public* surface never
    /// hands out a `CheckedQueryNode` that a caller could reassemble.
    pub(crate) fn into_parts(self) -> (CheckedQueryNode, Origin) {
        (self.node, self.origin)
    }

    /// Which stage built this query, without exposing the node type.
    ///
    /// A consumer outside the crate that needs to branch on the shape reads
    /// this rather than matching the node: the node enum is `pub(crate)`
    /// precisely so that a caller cannot construct one and bypass the
    /// constructors' checks.
    pub fn kind(&self) -> QueryKind {
        match &self.node {
            CheckedQueryNode::Table { .. } => QueryKind::Table,
            CheckedQueryNode::Where { .. } => QueryKind::Where,
            CheckedQueryNode::Select { .. } => QueryKind::Select,
            CheckedQueryNode::Update { .. } => QueryKind::Update,
            CheckedQueryNode::Omit { .. } => QueryKind::Omit,
            CheckedQueryNode::Rename { .. } => QueryKind::Rename,
            CheckedQueryNode::Agg { .. } => QueryKind::Agg,
            CheckedQueryNode::Order { .. } => QueryKind::Order,
            CheckedQueryNode::Limit { .. } => QueryKind::Limit,
            CheckedQueryNode::Offset { .. } => QueryKind::Offset,
            CheckedQueryNode::Distinct { .. } => QueryKind::Distinct,
            CheckedQueryNode::Join { .. } => QueryKind::Join,
            CheckedQueryNode::Set { .. } => QueryKind::Set,
        }
    }
    /// A table, with its columns known.
    ///
    /// `columns` is the table's closed row: the checker gives a `table`
    /// definition its column list from its signature, and a table whose
    /// columns are not known cannot be a checked query — every later stage
    /// would have to guess. `None` is rejected here rather than deferred, with
    /// the same message `schema` gives.
    pub fn table(
        schema: impl Into<String>,
        name: impl Into<String>,
        columns: Option<RowType>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let schema = schema.into();
        let name = name.into();
        let Some(row) = columns else {
            return Err(
                Error::new(crate::schema::unknown_table_columns(&schema, &name)).at(origin),
            );
        };
        // Read the names before `row` is moved into the node.
        let names = row.names();
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Table {
                schema,
                name,
                columns: names,
            },
            origin,
        })
    }

    /// `where pred`: a row-phase `bool` predicate; the output row is the
    /// input's.
    pub fn where_(input: CheckedQuery, pred: CheckedExpr, origin: Origin) -> Result<Self, Error> {
        place(Place::Where, &pred, origin)?;
        if !pred.ty.is_bool() {
            return Err(Error::new(format!(
                "a `where` predicate must be bool, found {}",
                pred.ty
            ))
            .at(origin));
        }
        refs_columns(&pred, &input.row, "where", origin)?;
        let row = Arc::clone(&input.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Where {
                input: Box::new(input),
                pred,
            },
            origin,
        })
    }

    /// `select {..}`: at least one field, every field row-phase and over the
    /// input row; the output row is the fields, in order.
    pub fn select(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let row = project_row("select", &input.row, &fields, Place::Select, origin)?;
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Select {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `update {..}`: like `select`, but merged over the input row — a listed
    /// name keeps its position and takes the **new** type, a new name is
    /// appended.
    ///
    /// The merge here is [`RowType::overwrite`] (right-wins), *not*
    /// [`RowType::merge`] (left-wins, the join law). Using the join law would
    /// silently keep the old type of an overwritten column while the checker
    /// and the erasure both say the new one.
    pub fn update(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let updated = project_row("update", &input.row, &fields, update_place(), origin)?;
        let row = input.row.overwrite(&updated);
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Update {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `agg {..}`: at least one field, every field at `Agg` or `Const` phase.
    /// A `Win` field is rejected: it belongs in a `select` after the
    /// aggregation.
    pub fn agg(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let row = project_row("agg", &input.row, &fields, Place::Agg, origin)?;
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Agg {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `order [..]`: row-phase sort keys over the input row; the output row
    /// is unchanged.
    pub fn order(
        input: CheckedQuery,
        keys: Vec<(CheckedExpr, bool)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        for (k, _) in &keys {
            place(Place::Key, k, origin)?;
            refs_columns(k, &input.row, "order", origin)?;
        }
        let row = Arc::clone(&input.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Order {
                input: Box::new(input),
                keys,
            },
            origin,
        })
    }

    /// `limit n`.
    pub fn limit(input: CheckedQuery, n: i64, origin: Origin) -> Result<Self, Error> {
        if n < 0 {
            return Err(
                Error::new(format!("`limit` needs a non-negative count, got {n}")).at(origin),
            );
        }
        let row = Arc::clone(&input.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Limit {
                input: Box::new(input),
                n,
            },
            origin,
        })
    }

    /// `offset n`.
    pub fn offset(input: CheckedQuery, n: i64, origin: Origin) -> Result<Self, Error> {
        if n < 0 {
            return Err(
                Error::new(format!("`offset` needs a non-negative count, got {n}")).at(origin),
            );
        }
        let row = Arc::clone(&input.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Offset {
                input: Box::new(input),
                n,
            },
            origin,
        })
    }

    /// `distinct`.
    pub fn distinct(input: CheckedQuery, origin: Origin) -> Result<Self, Error> {
        let row = Arc::clone(&input.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Distinct(Box::new(input)),
            origin,
        })
    }

    /// `omit "k"`: the input row without column `k`.
    ///
    /// A key the input does not have is **rejected here**, with the wording
    /// `schema::omit_columns` uses. [`RowType::omit`] is total because it is
    /// the row *former*, but this constructor is where the rule lives: a
    /// `CheckedQuery` that exists must be one `schema::schema` accepts, and
    /// `omit` on a missing key is exactly the case where a total row former
    /// would otherwise let an invalid query into the checked layer.
    pub fn omit(
        input: CheckedQuery,
        key: impl Into<String>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let key = key.into();
        if !input.row.has(&key) {
            let avail = input.row.names().join(", ");
            return Err(Error::new(format!("no column `{key}`; available: {avail}")).at(origin));
        }
        let row = input.row.omit(&key);
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Omit {
                input: Box::new(input),
                key,
            },
            origin,
        })
    }

    /// `prefix "s"` — every column name gains `affix` in front.
    pub fn prefix(
        input: CheckedQuery,
        affix: impl Into<String>,
        origin: Origin,
    ) -> Result<Self, Error> {
        Self::rename(input, affix, true, origin)
    }

    /// `suffix "s"` — every column name gains `affix` at the end.
    pub fn suffix(
        input: CheckedQuery,
        affix: impl Into<String>,
        origin: Origin,
    ) -> Result<Self, Error> {
        Self::rename(input, affix, false, origin)
    }

    /// The shared body of `prefix`/`suffix`. It keeps the direction explicit
    /// so a stale mapper cannot make `suffix` act as a prefix.
    pub fn rename(
        input: CheckedQuery,
        affix: impl Into<String>,
        prefix: bool,
        origin: Origin,
    ) -> Result<Self, Error> {
        let affix = affix.into();
        let row = input.row.rename_all(&affix, prefix);
        Ok(CheckedQuery {
            row: Arc::new(row),
            node: CheckedQueryNode::Rename {
                input: Box::new(input),
                affix,
                prefix,
            },
            origin,
        })
    }

    /// A join.
    ///
    /// The predicate must be row-phase `bool`, and every side-qualified name
    /// must come from the input its side names: `Side::Single` is rejected (a
    /// join predicate must say which input a column comes from), and a column
    /// absent from its side is reported with what that side has.
    ///
    /// The output row is the left input's, then the right's that the left does
    /// not have, with the nullable side's *types* wrapped in `maybe`: left join
    /// → right side, right join → left side, full → both. A semi/anti join
    /// keeps the left input's row.
    pub fn join(
        kind: JoinKind,
        left: CheckedQuery,
        right: CheckedQuery,
        on: CheckedExpr,
        origin: Origin,
    ) -> Result<Self, Error> {
        place(Place::JoinOn, &on, origin)?;
        if !on.ty.is_bool() {
            return Err(
                Error::new(format!("a join predicate must be bool, found {}", on.ty)).at(origin),
            );
        }
        for (side, n) in on.node.columns() {
            let (cols, what) = match side {
                Side::Left => (&left.row, "left"),
                Side::Right => (&right.row, "right"),
                Side::Single => return Err(Error::new(rules::needs_side(&n)).at(origin)),
            };
            if !cols.has(&n) {
                let avail = cols.names().join(", ");
                return Err(Error::new(format!(
                    "the {what} join input has no column `{n}`; available: {avail}"
                ))
                .at(origin));
            }
        }
        let row = match kind {
            JoinKind::Semi | JoinKind::Anti => Arc::clone(&left.row),
            _ => Arc::new(join_row(kind, &left.row, &right.row)),
        };
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Join {
                kind,
                left: Box::new(left),
                right: Box::new(right),
                on,
            },
            origin,
        })
    }

    /// A set operation: both inputs must expose the same row — the same names,
    /// in the same order, with the same types.
    pub fn set(
        kind: SetKind,
        left: CheckedQuery,
        right: CheckedQuery,
        origin: Origin,
    ) -> Result<Self, Error> {
        if left.row.names() != right.row.names() {
            return Err(Error::new(format!(
                "set-operation inputs must have the same columns; left has [{}], right has [{}]",
                left.row.names().join(", "),
                right.row.names().join(", ")
            ))
            .at(origin));
        }
        if left.row.columns != right.row.columns {
            let differing = left
                .row
                .columns()
                .iter()
                .zip(right.row.columns())
                .find(|(l, r)| l != r)
                .map(|(l, r)| {
                    format!(
                        "column `{}` is {} on the left but {} on the right",
                        l.0, l.1, r.1
                    )
                })
                .unwrap_or_else(|| "the two inputs differ".into());
            return Err(Error::new(format!(
                "set-operation inputs must have the same column types; {differing}"
            ))
            .at(origin));
        }
        let row = Arc::clone(&left.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Set {
                kind,
                left: Box::new(left),
                right: Box::new(right),
            },
            origin,
        })
    }

    /// The query's output row as a plain column list, for a comparison
    /// against `schema::schema(&erase(q))`.
    pub fn names(&self) -> Vec<String> {
        self.row.names()
    }

    /// Lower this query to the current relational IR (see [`erase`]).
    ///
    /// Infallible because a `CheckedQuery` has no expression in it yet: every
    /// `CheckedExpr` is attached by a node's fields, and this walks them. Use
    /// [`erase`] directly if you need the `Result`.
    ///
    /// # Panics
    ///
    /// Only if a `CheckedExprNode::Call` was reached, which is a compiler bug:
    /// calls are meant to be inlined or reduced before erasure. `erase` returns
    /// the error instead.
    pub fn erase(self) -> Rel {
        erase(self).unwrap_or_else(|e| panic!("internal error: {e}"))
    }

    /// `schema::schema` run over this query's erasure.
    pub fn erased_schema(&self) -> Result<Vec<String>, String> {
        crate::schema::schema(&erase(self.clone()).map_err(|e| e.message)?)
    }
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

/// A checked query's node. Each variant mirrors the `Rel` variant of the same
/// name, plus the row it produces (carried by [`CheckedQuery`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckedQueryNode {
    Table {
        schema: String,
        name: String,
        columns: Vec<String>,
    },
    Where {
        input: Box<CheckedQuery>,
        pred: CheckedExpr,
    },
    Select {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Update {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Omit {
        input: Box<CheckedQuery>,
        key: String,
    },
    /// `prefix`/`suffix`: one node with the direction, because both erase to a
    /// rename and nothing downstream cares which spelling produced it.
    Rename {
        input: Box<CheckedQuery>,
        affix: String,
        prefix: bool,
    },
    Agg {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Order {
        input: Box<CheckedQuery>,
        keys: Vec<(CheckedExpr, bool)>,
    },
    Limit {
        input: Box<CheckedQuery>,
        n: i64,
    },
    Offset {
        input: Box<CheckedQuery>,
        n: i64,
    },
    Distinct(Box<CheckedQuery>),
    Join {
        kind: JoinKind,
        left: Box<CheckedQuery>,
        right: Box<CheckedQuery>,
        on: CheckedExpr,
    },
    Set {
        kind: SetKind,
        left: Box<CheckedQuery>,
        right: Box<CheckedQuery>,
    },
}

// ── the checked expression ─────────────────────────────────────────────────

/// A checked expression: its phase, its scalar type, its node, its origin.
///
/// Private fields for the same reason as [`CheckedQuery`]: the phase, the
/// scalar type and the node have to agree — an `agg`-phase node carrying a
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
    /// [`CheckedQuery::into_parts`].
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

    /// A use of a checked definition, with the instantiation chosen for its
    /// overload holes. This is what makes an overload choice explicit at the
    /// call site instead of a dynamic lookup during evaluation.
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
fn place(place: Place, e: &CheckedExpr, origin: Origin) -> Result<(), Error> {
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
fn refs_columns(e: &CheckedExpr, input: &RowType, ctx: &str, origin: Origin) -> Result<(), Error> {
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
fn project_row(
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
fn update_place() -> Place {
    Place::Select
}

// ── erasure to the existing IR ─────────────────────────────────────────────

/// Lower a checked query to the current relational IR.
///
/// Structural: every checked constructor erases to its corresponding `Rel`
/// node, which `schema::schema` and the SQL backend consume.
///
/// Fallible, but only through [`erase_expr`]: a `CheckedExprNode::Call` has no
/// SQL expression, so a query containing one cannot be erased. No *query* node
/// can fail — the query side is total by construction, which is what the
/// `Result` makes visible rather than assumed.
///
/// Each node is stamped with `Rel::At(origin)`, except where the node is
/// already located, so nested source locations are preserved.
pub fn erase(query: CheckedQuery) -> Result<Rel, Error> {
    let (node, origin) = query.into_parts();
    let loc = Loc {
        module: origin.module,
        span: origin.span,
    };
    let at = |r: Rel| match r {
        r @ Rel::At(..) => r,
        r => Rel::At(loc, Box::new(r)),
    };
    let rel = match node {
        CheckedQueryNode::Table {
            schema,
            name,
            columns,
        } => at(Rel::Table {
            schema,
            name,
            columns: Some(columns),
        }),
        CheckedQueryNode::Where { input, pred } => {
            at(Rel::Where(Box::new(erase(*input)?), erase_expr(pred)?))
        }
        CheckedQueryNode::Select { input, fields } => {
            at(Rel::Select(Box::new(erase(*input)?), erase_fields(fields)?))
        }
        CheckedQueryNode::Update { input, fields } => {
            at(Rel::Update(Box::new(erase(*input)?), erase_fields(fields)?))
        }
        CheckedQueryNode::Omit { input, key } => at(Rel::Omit(Box::new(erase(*input)?), key)),
        CheckedQueryNode::Rename {
            input,
            affix,
            prefix,
        } => {
            let inner = Box::new(erase(*input)?);
            at(if prefix {
                Rel::Prefix(inner, affix)
            } else {
                Rel::Suffix(inner, affix)
            })
        }
        CheckedQueryNode::Agg { input, fields } => {
            at(Rel::Agg(Box::new(erase(*input)?), erase_fields(fields)?))
        }
        CheckedQueryNode::Order { input, keys } => at(Rel::Order(
            Box::new(erase(*input)?),
            keys.into_iter()
                .map(|(e, asc)| Ok((erase_expr(e)?, asc)))
                .collect::<Result<Vec<_>, Error>>()?,
        )),
        CheckedQueryNode::Limit { input, n } => at(Rel::Limit(Box::new(erase(*input)?), n)),
        CheckedQueryNode::Offset { input, n } => at(Rel::Offset(Box::new(erase(*input)?), n)),
        CheckedQueryNode::Distinct(input) => at(Rel::Distinct(Box::new(erase(*input)?))),
        CheckedQueryNode::Join {
            kind,
            left,
            right,
            on,
        } => at(Rel::Join {
            kind,
            left: Box::new(erase(*left)?),
            right: Box::new(erase(*right)?),
            on: erase_expr(on)?,
        }),
        CheckedQueryNode::Set { kind, left, right } => at(Rel::Set {
            kind,
            left: Box::new(erase(*left)?),
            right: Box::new(erase(*right)?),
        }),
    };
    Ok(rel)
}

/// Erase a list of checked expressions (a projection's fields), stopping at the
/// first that cannot be erased.
fn erase_fields(fields: Vec<(String, CheckedExpr)>) -> Result<Vec<(String, Expr)>, Error> {
    fields
        .into_iter()
        .map(|(n, e)| Ok((n, erase_expr(e)?)))
        .collect()
}

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
                 inlined or reduced before a checked query is erased"
            )))
        }
    };
    Ok(expr)
}

/// Erase a list of checked expressions, stopping at the first that cannot be.
fn erase_exprs(es: Vec<CheckedExpr>) -> Result<Vec<Expr>, Error> {
    es.into_iter().map(erase_expr).collect()
}

/// Erase, then run `schema::schema` over the result. The constructors have
/// already proved what this validator re-derives, so it is a debug check.
pub fn erase_checked(query: CheckedQuery) -> Result<(Rel, Vec<String>), String> {
    let rel = erase(query).map_err(|e| e.message)?;
    let cols = crate::schema::schema(&rel)?;
    Ok((rel, cols))
}

#[cfg(test)]
mod tests;
