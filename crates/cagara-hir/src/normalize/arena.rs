//! The typed relational arena: the one place relational validity lives.
//!
//! Every query the compiler builds is a graph of [`RelNode`]s. A node is built
//! by the operation it is — a table, a stage, a join, a set — and its
//! constructor *is* that operation's rule: a filter takes a boolean predicate
//! over the input's columns, a projection computes its output row from the
//! fields it was given, a join wraps the nullable side's types, a set operation
//! requires equal rows.
//!
//! # Why an arena and not a tree
//!
//! A pipeline is a chain, not a nesting: `users & where p & select f` nests the
//! *applications*, but the three stages are three separate operations, each
//! consuming the previous one's output. As a graph the chain is the children
//! (`inputs`), and a consumer reads the query by following them without
//! reconstructing anything. A query definition used twice is normalized once per
//! use — the checker recorded two sites — and both uses name nodes in the same
//! arena, so nothing has to copy a sub-graph into its caller.
//!
//! # What a node records and what it does not
//!
//! * its **kind** — the operation, not the spelling;
//! * its **site** — the node that applied it, stable across inlining, so a stage
//!   keeps the identity the checker gave it;
//! * its **origin** — where it was written, so a diagnostic names the stage;
//! * its **children**, in operand order (the piped input last, as the prelude
//!   declares it);
//! * its **row**, computed by the constructor — never recovered from a type
//!   after the fact;
//! * its **payload** — the expressions it owns; there is exactly one payload
//!   shape per operation, chosen by the constructor, so a node cannot carry a
//!   payload its kind does not mean.
//!
//! It does not record a type. Erasure ([`CompiledQuery::erase`]) walks the graph
//! structurally and emits [`crate::ir::Rel`], which is what the SQL backend
//! sees; nothing downstream can re-type-check it.
//!
//! # Payment for a node
//!
//! A builder that runs out of normalization budget between two nodes would leave
//! an arena that is half a query. [`Builder::charge`] is how the normalizer pays
//! for a node *before* it builds it, so a refused normalization throws the arena
//! away whole instead of returning a prefix of one.

use crate::check::NodeRef;
use crate::checked::CheckedExpr;
use crate::core::{Error, Origin, RowType, ScalarType};
use crate::ir::{JoinKind, SetKind};
use crate::rules::Place;

/// A handle to one node of a [`Builder`].
///
/// Opaque on purpose: it is an index into one arena, and the only way to get
/// one is from a constructor of that arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(u32);

impl NodeId {
    fn at(index: usize) -> Self {
        NodeId(u32::try_from(index).expect("a query has fewer than 2^32 nodes"))
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

/// The operation a node applied.
///
/// `Prefix` and `Suffix` are separate variants because their payloads differ;
/// both erase to the same rename, and nothing downstream reads which spelling
/// was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum QueryNodeKind {
    Table,
    Prefix,
    Suffix,
    Omit,
    MapValue,
    Where,
    Select,
    Update,
    Agg,
    Order,
    Limit,
    Offset,
    Distinct,
    Join(JoinKind),
    Set(SetKind),
}

#[allow(dead_code)] // Introspection kept for debug and compatibility tests.
impl QueryNodeKind {
    /// How many relational children the operation takes.
    pub(crate) fn arity(self) -> usize {
        match self {
            QueryNodeKind::Table => 0,
            QueryNodeKind::Join(_) | QueryNodeKind::Set(_) => 2,
            _ => 1,
        }
    }
}

/// What a node owns besides its children.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Payload {
    /// A table: the only leaf.
    Table {
        schema: String,
        name: String,
        columns: Vec<String>,
    },
    /// `where pred`.
    Predicate(CheckedExpr),
    /// `select`/`update`/`agg` fields, in source order.
    Fields(Vec<(String, CheckedExpr)>),
    /// `order` keys, in source order.
    Keys(Vec<(CheckedExpr, bool)>),
    /// `prefix "s"` / `suffix "s"`.
    Affix { affix: String, prefix: bool },
    /// `omit "k"`.
    Key(String),
    /// `mapValue "w"`.
    Wrapper(String),
    /// `limit n` / `offset n`.
    Count(i64),
    /// A join's `on`.
    On(CheckedExpr),
    /// A set operation and a distinct carry no payload.
    None,
}

#[allow(dead_code)] // Introspection kept for debug and compatibility tests.
impl Payload {
    /// The prefix/suffix direction, for a consumer that needs it.
    pub(crate) fn is_prefix(&self) -> Option<bool> {
        match self {
            Payload::Affix { prefix, .. } => Some(*prefix),
            _ => None,
        }
    }
}

/// One node of the arena.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RelNode {
    pub(crate) kind: QueryNodeKind,
    pub(crate) site: NodeRef,
    pub(crate) origin: Origin,
    pub(crate) inputs: Vec<NodeId>,
    pub(crate) row: RowType,
    pub(crate) payload: Payload,
}

/// A query being built: the arena, and the budget it is built under.
///
/// The `budget` field is the normalizer's, threaded here so one object owns both
/// halves of "construct a node": the cost of doing it and the node. There is no
/// way to make a [`CompiledQuery`] except [`Builder::finish`], which takes the
/// root the definition normalized to, so a finished query has at least one node
/// and a real root.
#[derive(Debug)]
pub(crate) struct Builder {
    nodes: Vec<RelNode>,
    budget: Option<crate::normalize::budget::Budget>,
}

impl Clone for Builder {
    fn clone(&self) -> Self {
        Builder {
            nodes: self.nodes.clone(),
            budget: self.budget.clone(),
        }
    }
}

impl Builder {
    pub(crate) fn new() -> Self {
        Builder {
            nodes: Vec::new(),
            budget: None,
        }
    }

    /// Build under `budget`; every node construction is charged to it.
    #[allow(dead_code)] // Direct arena tests can opt into a shared budget.
    pub(crate) fn with_budget(mut self, budget: crate::normalize::budget::Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Pay for one node before constructing it.
    ///
    /// Only the normalizer charges; a test that constructs nodes directly pays
    /// nothing, which is what makes the constructors usable as plain rules.
    pub(crate) fn charge(&mut self, origin: Origin) -> Result<(), crate::normalize::Control> {
        match &mut self.budget {
            Some(budget) => budget.spend_node(origin),
            None => Ok(()),
        }
    }

    pub(crate) fn node(&self, id: NodeId) -> &RelNode {
        &self.nodes[id.index()]
    }

    /// The row a node produces.
    #[allow(dead_code)] // Used by debug consumers of the arena.
    pub(crate) fn row(&self, id: NodeId) -> &RowType {
        &self.node(id).row
    }

    fn push(&mut self, node: RelNode) -> NodeId {
        let id = NodeId::at(self.nodes.len());
        self.nodes.push(node);
        id
    }

    /// A **leaf**: a table, with its columns known.
    ///
    /// `None` for the columns is rejected here and now. The checker gives a
    /// table definition its column list from its signature, so a table whose
    /// columns are not known is a program error rather than a stage that could
    /// be deferred — and every later stage over it would have to guess.
    pub(crate) fn table(
        &mut self,
        site: NodeRef,
        origin: Origin,
        schema: impl Into<String>,
        name: impl Into<String>,
        columns: Option<Vec<(String, ScalarType)>>,
    ) -> Result<NodeId, Error> {
        let schema = schema.into();
        let name = name.into();
        let Some(row) = columns else {
            return Err(
                Error::new(crate::schema::unknown_table_columns(&schema, &name)).at(origin),
            );
        };
        let row = RowType::new(row);
        let names = row.names();
        Ok(self.push(RelNode {
            kind: QueryNodeKind::Table,
            site,
            origin,
            inputs: Vec::new(),
            row,
            payload: Payload::Table {
                schema,
                name,
                columns: names,
            },
        }))
    }

    /// A **row rewrite**: `prefix`, `suffix`, `omit`, `mapValue`.
    ///
    /// The output row is the operation's own rule applied to the input row, so
    /// it is computed here, once. A rewrite that cannot apply — no such
    /// column, an unknown wrapper — is refused with the program's wording.
    pub(crate) fn rewrite(
        &mut self,
        site: NodeRef,
        origin: Origin,
        input: NodeId,
        rewrite: Rewrite,
    ) -> Result<NodeId, Error> {
        let input_row = self.node(input).row.clone();
        let (kind, payload, row) = match rewrite {
            Rewrite::Rename { affix, prefix } => {
                let row = input_row.rename_all(&affix, prefix);
                let kind = if prefix {
                    QueryNodeKind::Prefix
                } else {
                    QueryNodeKind::Suffix
                };
                (kind, Payload::Affix { affix, prefix }, row)
            }
            Rewrite::Omit(key) => {
                if !input_row.has(&key) {
                    let avail = input_row.names().join(", ");
                    return Err(
                        Error::new(format!("no column `{key}`; available: {avail}")).at(origin)
                    );
                }
                let row = input_row.omit(&key);
                (QueryNodeKind::Omit, Payload::Key(key), row)
            }
            Rewrite::MapValue(wrapper) => {
                let row = match wrapper.as_str() {
                    "nullable" => input_row.map_value_nullable(),
                    "list" => RowType::new(
                        input_row
                            .columns()
                            .iter()
                            .map(|(name, ty)| {
                                (name.clone(), ScalarType::List(Box::new(ty.clone())))
                            })
                            .collect(),
                    ),
                    _ => {
                        return Err(
                            Error::new(format!("unknown value wrapper `{wrapper}`")).at(origin)
                        )
                    }
                };
                (QueryNodeKind::MapValue, Payload::Wrapper(wrapper), row)
            }
        };
        Ok(self.push(RelNode {
            kind,
            site,
            origin,
            inputs: vec![input],
            row,
            payload,
        }))
    }

    /// A stage that **keeps its input's row**: `where`, `order`, `limit`,
    /// `offset`, `distinct`.
    ///
    /// Keeping the row is the whole content of the rule for these stages, so
    /// this is the one place a row is derived by identity rather than by an
    /// operation.
    pub(crate) fn keep_row(
        &mut self,
        site: NodeRef,
        origin: Origin,
        input: NodeId,
        kept: Kept,
    ) -> Result<NodeId, Error> {
        let row = self.node(input).row.clone();
        let (kind, payload) = match kept {
            Kept::Filter(pred) => {
                crate::checked::place(Place::Where, &pred, origin)?;
                if !pred.ty().is_bool() {
                    return Err(Error::new(format!(
                        "a `where` predicate must be bool, found {}",
                        pred.ty()
                    ))
                    .at(origin));
                }
                crate::checked::refs_columns(&pred, &row, "where", origin)?;
                (QueryNodeKind::Where, Payload::Predicate(pred))
            }
            Kept::Order(keys) => {
                for (key, _) in &keys {
                    crate::checked::place(Place::Key, key, origin)?;
                    crate::checked::refs_columns(key, &row, "order", origin)?;
                }
                (QueryNodeKind::Order, Payload::Keys(keys))
            }
            Kept::Limit(n) => {
                if n < 0 {
                    return Err(
                        Error::new(format!("`limit` needs a non-negative count, got {n}"))
                            .at(origin),
                    );
                }
                (QueryNodeKind::Limit, Payload::Count(n))
            }
            Kept::Offset(n) => {
                if n < 0 {
                    return Err(Error::new(format!(
                        "`offset` needs a non-negative count, got {n}"
                    ))
                    .at(origin));
                }
                (QueryNodeKind::Offset, Payload::Count(n))
            }
            Kept::Distinct => (QueryNodeKind::Distinct, Payload::None),
        };
        Ok(self.push(RelNode {
            kind,
            site,
            origin,
            inputs: vec![input],
            row,
            payload,
        }))
    }

    /// A **projection**: `select`, `update`, `agg`.
    ///
    /// The output row is built from the field expressions, so it is computed
    /// here from the payload — the same expressions the SQL will read.
    pub(crate) fn project(
        &mut self,
        site: NodeRef,
        origin: Origin,
        input: NodeId,
        projection: Projection,
        fields: Vec<(String, CheckedExpr)>,
    ) -> Result<NodeId, Error> {
        let input_row = self.node(input).row.clone();
        let (kind, row) = match projection {
            Projection::Select => (
                QueryNodeKind::Select,
                crate::checked::project_row("select", &input_row, &fields, Place::Select, origin)?,
            ),
            Projection::Update => {
                let updated = crate::checked::project_row(
                    "update",
                    &input_row,
                    &fields,
                    crate::checked::update_place(),
                    origin,
                )?;
                (QueryNodeKind::Update, input_row.overwrite(&updated))
            }
            Projection::Agg => (
                QueryNodeKind::Agg,
                crate::checked::project_row("agg", &input_row, &fields, Place::Agg, origin)?,
            ),
        };
        Ok(self.push(RelNode {
            kind,
            site,
            origin,
            inputs: vec![input],
            row,
            payload: Payload::Fields(fields),
        }))
    }

    /// A **join**.
    ///
    /// The predicate must be a row-phase boolean, and every side-qualified name
    /// must come from the side it names — `.x` is refused, because a join
    /// predicate has to say which input a column belongs to. The output row is
    /// the left input's columns, then the right's that the left does not have,
    /// with the nullable sides' types wrapped in `maybe`: a left join makes the
    /// right side nullable, a right join the left, a full join both, and a
    /// semi/anti join keeps the left input's row alone.
    pub(crate) fn join(
        &mut self,
        site: NodeRef,
        origin: Origin,
        kind: JoinKind,
        left: NodeId,
        right: NodeId,
        on: CheckedExpr,
    ) -> Result<NodeId, Error> {
        crate::checked::place(Place::JoinOn, &on, origin)?;
        if !on.ty().is_bool() {
            return Err(
                Error::new(format!("a join predicate must be bool, found {}", on.ty())).at(origin),
            );
        }
        let left_row = self.node(left).row.clone();
        let right_row = self.node(right).row.clone();
        for (side, name) in columns_of(&on) {
            let (columns, what) = match side {
                cagara_syntax::ast::Side::Left => (&left_row, "left"),
                cagara_syntax::ast::Side::Right => (&right_row, "right"),
                cagara_syntax::ast::Side::Single => {
                    return Err(Error::new(crate::rules::needs_side(&name)).at(origin))
                }
            };
            if !columns.has(&name) {
                let avail = columns.names().join(", ");
                return Err(Error::new(format!(
                    "the {what} join input has no column `{name}`; available: {avail}"
                ))
                .at(origin));
            }
        }
        let row = join_row(kind, &left_row, &right_row);
        Ok(self.push(RelNode {
            kind: QueryNodeKind::Join(kind),
            site,
            origin,
            inputs: vec![left, right],
            row,
            payload: Payload::On(on),
        }))
    }

    /// A **set operation**: both inputs must expose the same row — the same
    /// names, in the same order, with the same types.
    pub(crate) fn set(
        &mut self,
        site: NodeRef,
        origin: Origin,
        kind: SetKind,
        left: NodeId,
        right: NodeId,
    ) -> Result<NodeId, Error> {
        let left_row = self.node(left).row.clone();
        let right_row = self.node(right).row.clone();
        if left_row.names() != right_row.names() {
            return Err(Error::new(format!(
                "set-operation inputs must have the same columns; left has [{}], right has [{}]",
                left_row.names().join(", "),
                right_row.names().join(", ")
            ))
            .at(origin));
        }
        if left_row.columns() != right_row.columns() {
            let differing = left_row
                .columns()
                .iter()
                .zip(right_row.columns())
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
        Ok(self.push(RelNode {
            kind: QueryNodeKind::Set(kind),
            site,
            origin,
            inputs: vec![left, right],
            row: left_row,
            payload: Payload::None,
        }))
    }

    /// Finish a query at `root`.
    pub(crate) fn finish(self, root: NodeId) -> CompiledQuery {
        CompiledQuery {
            nodes: self.nodes,
            root,
        }
    }
}

/// Which row rewrite a [`Builder::rewrite`] builds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Rewrite {
    Rename { affix: String, prefix: bool },
    Omit(String),
    MapValue(String),
}

/// Which row-preserving stage a [`Builder::keep_row`] builds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kept {
    Filter(CheckedExpr),
    Order(Vec<(CheckedExpr, bool)>),
    Limit(i64),
    Offset(i64),
    Distinct,
}

/// Which projection a [`Builder::project`] builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Projection {
    Select,
    Update,
    Agg,
}

/// A finished query: the arena and the node the definition normalized to.
#[allow(dead_code)] // Read by the erase boundary, which lands with the route cut.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompiledQuery {
    nodes: Vec<RelNode>,
    root: NodeId,
}

/// A read-only view of one node.
#[allow(dead_code)] // Read by debug consumers of the arena.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueryNode<'a> {
    id: NodeId,
    node: &'a RelNode,
}

#[allow(dead_code)] // Read by debug consumers of the arena.
impl<'a> QueryNode<'a> {
    pub(crate) fn id(&self) -> NodeId {
        self.id
    }
    pub(crate) fn kind(&self) -> QueryNodeKind {
        self.node.kind
    }
    pub(crate) fn site(&self) -> NodeRef {
        self.node.site
    }
    pub(crate) fn origin(&self) -> Origin {
        self.node.origin
    }
    pub(crate) fn inputs(&self) -> &[NodeId] {
        &self.node.inputs
    }
    pub(crate) fn input(&self) -> Option<NodeId> {
        self.node.inputs.first().copied()
    }
    pub(crate) fn row(&self) -> &RowType {
        &self.node.row
    }
    pub(crate) fn payload(&self) -> &Payload {
        &self.node.payload
    }
}

#[allow(dead_code)] // Read by debug consumers of the arena.
impl CompiledQuery {
    /// The node the definition's body produced.
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    /// The output row of the definition's query.
    pub(crate) fn row(&self) -> &RowType {
        &self.nodes[self.root.index()].row
    }

    pub(crate) fn node(&self, id: NodeId) -> QueryNode<'_> {
        QueryNode {
            id,
            node: &self.nodes[id.index()],
        }
    }

    /// Every node, in construction order.
    pub(crate) fn nodes(&self) -> impl Iterator<Item = QueryNode<'_>> {
        self.nodes
            .iter()
            .enumerate()
            .map(move |(index, node)| QueryNode {
                id: NodeId::at(index),
                node,
            })
    }

    /// The query's own chain, root first: the nodes reached by following the
    /// first input of each.
    pub(crate) fn chain(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut next = Some(self.root);
        while let Some(id) = next {
            out.push(id);
            next = self.nodes[id.index()].inputs.first().copied();
        }
        out
    }

    /// The query's output columns, in order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.row().names()
    }
}

// ── shared rules ───────────────────────────────────────────────────────────

use crate::checked::join_row;

/// Every column a checked expression reads, in reading order.
fn columns_of(expr: &CheckedExpr) -> Vec<(cagara_syntax::ast::Side, String)> {
    let mut out = Vec::new();
    expr.columns(&mut out);
    out
}

// ── erasure ────────────────────────────────────────────────────────────────

impl CompiledQuery {
    /// Lower this query to the relational IR the SQL backend reads.
    ///
    /// Structural: every constructor's payload has a `Rel` node of the same
    /// name, and this walks the graph emitting it. It re-checks nothing — the
    /// constructors already did, which is what makes erasure cheap and what
    /// makes "SQL lowering cannot become a second type checker" true.
    ///
    /// Fallible only through a checked expression that cannot be erased (an
    /// unresolved `Call`, which is a compiler bug): the query side is total by
    /// construction.
    ///
    /// Every node is stamped with its own `Rel::At(origin)`, so a diagnostic
    /// downstream names the stage that wrote the node.
    pub(crate) fn erase(&self) -> Result<crate::ir::Rel, crate::core::Error> {
        self.erase_at(self.root)
    }

    /// Erase one node.
    pub(crate) fn erase_at(&self, id: NodeId) -> Result<crate::ir::Rel, crate::core::Error> {
        let node = &self.nodes[id.index()];
        let loc = crate::ir::Loc {
            module: node.origin.module,
            span: node.origin.span,
        };
        let at = |rel: crate::ir::Rel| match rel {
            rel @ crate::ir::Rel::At(..) => rel,
            rel => crate::ir::Rel::At(loc, Box::new(rel)),
        };
        let child = |index: usize| -> Result<Box<crate::ir::Rel>, crate::core::Error> {
            match node.inputs.get(index) {
                Some(&input) => Ok(Box::new(self.erase_at(input)?)),
                None => Err(crate::core::Error::new(format!(
                    "{:?} has no child {index}",
                    node.kind
                ))
                .at(node.origin)),
            }
        };
        let rel = match (&node.payload, node.kind) {
            (
                Payload::Table {
                    schema,
                    name,
                    columns,
                },
                _,
            ) => crate::ir::Rel::Table {
                schema: schema.clone(),
                name: name.clone(),
                columns: Some(columns.clone()),
            },
            (Payload::Predicate(pred), _) => {
                crate::ir::Rel::Where(child(0)?, crate::checked::erase_expr(pred.clone())?)
            }
            (Payload::Fields(fields), QueryNodeKind::Select) => {
                crate::ir::Rel::Select(child(0)?, erase_fields(fields)?)
            }
            (Payload::Fields(fields), QueryNodeKind::Update) => {
                crate::ir::Rel::Update(child(0)?, erase_fields(fields)?)
            }
            (Payload::Fields(fields), QueryNodeKind::Agg) => {
                crate::ir::Rel::Agg(child(0)?, erase_fields(fields)?)
            }
            (Payload::Keys(keys), _) => crate::ir::Rel::Order(
                child(0)?,
                keys.iter()
                    .map(|(key, ascending)| {
                        Ok((crate::checked::erase_expr(key.clone())?, *ascending))
                    })
                    .collect::<Result<Vec<_>, crate::core::Error>>()?,
            ),
            (Payload::Affix { affix, prefix }, _) => {
                let inner = child(0)?;
                if *prefix {
                    crate::ir::Rel::Prefix(inner, affix.clone())
                } else {
                    crate::ir::Rel::Suffix(inner, affix.clone())
                }
            }
            (Payload::Key(key), _) => crate::ir::Rel::Omit(child(0)?, key.clone()),
            // A value wrapper changes the checked row and has no SQL
            // representation, so erasure walks through it.
            (Payload::Wrapper(_), _) => {
                return self.erase_at(node.inputs[0]);
            }
            (Payload::Count(n), QueryNodeKind::Limit) => crate::ir::Rel::Limit(child(0)?, *n),
            (Payload::Count(n), QueryNodeKind::Offset) => crate::ir::Rel::Offset(child(0)?, *n),
            (Payload::None, QueryNodeKind::Distinct) => crate::ir::Rel::Distinct(child(0)?),
            (Payload::On(on), QueryNodeKind::Join(kind)) => crate::ir::Rel::Join {
                kind,
                left: child(0)?,
                right: child(1)?,
                on: crate::checked::erase_expr(on.clone())?,
            },
            (Payload::None, QueryNodeKind::Set(kind)) => crate::ir::Rel::Set {
                kind,
                left: child(0)?,
                right: child(1)?,
            },
            (payload, kind) => {
                return Err(crate::core::Error::new(format!(
                    "internal error: a {kind:?} node carries {payload:?}"
                ))
                .at(node.origin))
            }
        };
        Ok(at(rel))
    }
}

/// Erase a projection's fields, stopping at the first that cannot be erased.
fn erase_fields(
    fields: &[(String, CheckedExpr)],
) -> Result<Vec<(String, crate::ir::Expr)>, crate::core::Error> {
    fields
        .iter()
        .map(|(name, expr)| Ok((name.clone(), crate::checked::erase_expr(expr.clone())?)))
        .collect()
}
