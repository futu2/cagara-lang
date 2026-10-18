//! The **typed query**: a relational skeleton built while checking, whose nodes
//! own their boundaries.
//!
//! # Why this exists
//!
//! The production compiler now uses `normalize::arena` for query construction.
//! `CheckedQuery` and this typed arena remain as compatibility structures for
//! checked-tree APIs and route tests. They preserve the checker-owned stage
//! boundaries so those consumers do not have to recover them from a composed
//! result type.
//!
//! This module is the other representation. A [`TypedQueryNode`] is created
//! **where the stage is applied**, while the checker still has the operation and
//! its own signature instance in hand, and it records:
//!
//! * the node that applied the operation ([`NodeRef`] — `(module, definition,
//!   expr_id)`, stable across inlining and substitution);
//! * the kind of operation ([`QueryNodeKind`]);
//! * the **typed child** it was applied to, by [`NodeId`] — a handle into this
//!   arena, not a syntax node and not a term;
//! * the row the operation **read** and the row it **produced**, as the
//!   checker closed them at that node.
//!
//! The last two are the whole point. The redesign's invariant is that a stage's
//! boundary is its own signature instance, never a row recovered from a composed
//! result type — so the row has to be captured where the instance was bound,
//! which is checking, and read from there by whoever builds the final query.
//! Nothing here recomputes a row from a final query type, and nothing here
//! queries the final type at all: [`TypedQueryNode::output_row`] is the row the
//! node produced, and it is the *only* answer this type has about its output.
//!
//! # Relation to the typed expression tree
//!
//! [`crate::check::TypedBody`] is a tree over the *syntax*: one node per
//! expression, children in source order. This is a graph over the *query*, and
//! it is deliberately not the same thing. A pipeline's stages are siblings in
//! the syntax — `users & where p & select f` nests the applications, but the
//! stages themselves are three separate nodes — while as a query they form a
//! chain, each consuming the previous one's output. The `input` link is that
//! chain, and it is why this is its own arena rather than a projection of the
//! expression tree.
//!
//! # Scope
//!
//! Deliberately partial. The kinds here are the ones a query can be decomposed
//! into *structurally*; the expression arguments of the stages that take them
//! (`where`'s predicate, `select`'s fields, a join's `on`) stay on the existing
//! checked-expression path for now, and the constructors that need them
//! ([`TypedQuery::project`], the joins, the set operations) are built by later
//! slices. A kind with no builder yet is a kind nothing constructs, not a kind
//! with a guess in it.
//!
//! Because compatibility consumers arrive slice by slice, part of this module
//! is intentionally unused. The allowance below is scoped to the module and
//! says why, rather than being applied crate-wide.
#![allow(dead_code)]

use super::NodeRef;
use crate::checked::CheckedExpr;
use crate::core::{Origin, ScalarType};
use crate::ir::{JoinKind, SetKind};

/// A handle to one node of a [`TypedQuery`]'s arena.
///
/// Opaque on purpose: it is an index, and an index is only meaningful against
/// the arena that produced it. A caller that could build one could name a node
/// that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    fn at(index: usize) -> Self {
        NodeId(u32::try_from(index).expect("a query has fewer than 2^32 nodes"))
    }

    /// The arena position, for the arena's own use.
    fn index(self) -> usize {
        self.0 as usize
    }
}

/// A query's output row, as the checker closed it.
///
/// A newtype rather than a bare `Vec` so the two questions a reader has — "what
/// are the columns" and "is this row *known*" — cannot be confused. A row that
/// is still open (a helper's stage applied to a parameter) is `None` at the
/// node, which is different from a row with no columns; that distinction is the
/// same one [`crate::check::RowStageView`] draws and it is load-bearing, because
/// an open row is a row the checker has not finished deciding rather than one it
/// decided is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    columns: Vec<(String, ScalarType)>,
}

impl Row {
    pub fn new(columns: Vec<(String, ScalarType)>) -> Self {
        Row { columns }
    }

    /// The columns, in order.
    pub fn columns(&self) -> &[(String, ScalarType)] {
        &self.columns
    }

    /// The column names, in order.
    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The row as the checked layer's `RowType`.
    pub fn to_row_type(&self) -> crate::core::RowType {
        crate::core::RowType::new(self.columns.clone())
    }
}

/// What operation a typed query node applied.
///
/// One variant per *relational* operation, named for the operation rather than
/// for the spelling that reached it: `prefix` and `suffix` are one
/// [`QueryNodeKind::Rename`] carrying a direction, exactly as the checked layer
/// and the IR treat them, because nothing downstream cares which word was
/// written.
///
/// A node's kind is the *operation*, so a kind does not by itself say how many
/// children the node has: a `where` has one, a join has two, a table has none.
/// [`QueryNodeKind::arity`] states which, once, so the constructors can check it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryNodeKind {
    /// A table: no input, and the only leaf.
    Table,
    /// `prefix "s"`: every column name gains `s` in front.
    Prefix,
    /// `suffix "s"`: every column name gains `s` at the end.
    Suffix,
    /// `omit "k"`: the input row without column `k`.
    Omit,
    /// `mapValue w`: every field type of the input row wrapped by `w`.
    MapValue,
    /// `where pred`: the input row, filtered.
    Where,
    /// `select {..}`: a new row from the listed fields.
    Select,
    /// `update {..}`: the listed fields merged over the input row.
    Update,
    /// `agg {..}`: a grouped/aggregate row from the listed fields.
    Agg,
    /// `order [..]`: the input row, sorted.
    Order,
    /// `limit n`.
    Limit,
    /// `offset n`.
    Offset,
    /// `distinct`.
    Distinct,
    /// A two-input join.
    Join(JoinKind),
    /// A two-input set operation.
    Set(SetKind),
}

impl QueryNodeKind {
    /// How many relational children the operation takes.
    ///
    /// Stated here rather than inferred per constructor so the two cannot drift:
    /// a `where` that forgot to record its input would otherwise build a node
    /// that looks like a table with a predicate.
    pub fn arity(self) -> usize {
        match self {
            QueryNodeKind::Table => 0,
            QueryNodeKind::Join(_) | QueryNodeKind::Set(_) => 2,
            QueryNodeKind::Prefix
            | QueryNodeKind::Suffix
            | QueryNodeKind::Omit
            | QueryNodeKind::MapValue
            | QueryNodeKind::Where
            | QueryNodeKind::Select
            | QueryNodeKind::Update
            | QueryNodeKind::Agg
            | QueryNodeKind::Order
            | QueryNodeKind::Limit
            | QueryNodeKind::Offset
            | QueryNodeKind::Distinct => 1,
        }
    }

    /// Does this operation's output row follow from its input row and its
    /// witness alone?
    ///
    /// The four row *rewrites* do: a rename renames, a removal removes, a
    /// wrapper wraps. A projection or an aggregate builds its row from
    /// expressions the stage was given, so its output is not a function of its
    /// input and there is nothing to recompute.
    ///
    /// This is the predicate the checker's own stage invariant uses
    /// ([`crate::check::RowStage::recomputes`]), restated here for the query
    /// nodes, which are a different type. The two agree because they answer the
    /// same question about the same four operations; a slice that added a fifth
    /// rewrite would have to say so in both places, which is the correct amount
    /// of friction.
    pub fn recomputes_row(self) -> bool {
        matches!(
            self,
            QueryNodeKind::Prefix
                | QueryNodeKind::Suffix
                | QueryNodeKind::Omit
                | QueryNodeKind::MapValue
        )
    }
}

/// One node of a typed query: an operation, its typed child, and its rows.
///
/// Private to this module, and built only through the constructors below, so a
/// node that exists has an arity its kind agrees with and an origin. The fields
/// a later slice needs but this one does not yet populate — the expression
/// arguments of the stages that take them — are absent rather than defaulted, so
/// a slice that needs one has to add it deliberately instead of inheriting a
/// plausible wrong value.
///
/// A consumer outside this module reads it through the accessors on
/// [`TypedQuery`] and [`QueryNode`] below, which is what keeps "the constructors
/// are the only way to make one" a fact rather than a convention.
#[derive(Debug, Clone, PartialEq)]
struct TypedQueryNode {
    /// The node that applied the operation, stable across inlining.
    site: NodeRef,
    kind: QueryNodeKind,
    /// The typed child this operation was applied to, when it takes one.
    inputs: Vec<NodeId>,
    /// The row the operation read, as the checker closed it at this node.
    ///
    /// `None` when the row is still open — a helper's stage applied to a
    /// parameter — which is a different fact from a row with no columns.
    input_rows: Vec<Option<Row>>,
    /// The row the operation produced, as the checker closed it at this node.
    ///
    /// This is the answer to "what does this node produce", and there is no
    /// other: nothing here consults a final query type to work it out.
    output_row: Option<Row>,
    /// Where the operation was written.
    origin: Origin,
    /// The checked predicate owned by a `where` node, when this slice has
    /// carried expression payloads into the typed query.
    predicate: Option<CheckedExpr>,
    /// The checked sort keys owned by an `order` node.
    order_keys: Option<Vec<(CheckedExpr, bool)>>,
    /// The checked output fields owned by a projection or aggregate node.
    projection_fields: Option<Vec<(String, CheckedExpr)>>,
    /// The checked `on` expression owned by a join node.
    join_predicate: Option<CheckedExpr>,
}

/// A read-only view of one typed query node.
///
/// A borrow of the arena rather than a clone of the node, so a consumer that
/// walks a query is reading the nodes the constructors built. The fields are
/// private and reached through these accessors for the same reason
/// [`crate::checked::CheckedQuery`]'s are: a caller cannot pair one node's row
/// with another node's kind.
#[derive(Debug, Clone, Copy)]
pub struct QueryNode<'a> {
    id: NodeId,
    node: &'a TypedQueryNode,
}

impl<'a> QueryNode<'a> {
    /// This node's handle in its arena.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The application this node is, stable across inlining and substitution.
    pub fn site(&self) -> NodeRef {
        self.node.site
    }

    /// The operation this node applied.
    pub fn kind(&self) -> QueryNodeKind {
        self.node.kind
    }

    /// The typed child this operation was applied to, when it takes one.
    ///
    /// A [`NodeId`] into the same arena, so a caller follows the chain through
    /// the query rather than through syntax.
    pub fn input(&self) -> Option<NodeId> {
        self.node.inputs.first().copied()
    }

    /// All relational children in operation order.
    pub fn inputs(&self) -> &[NodeId] {
        &self.node.inputs
    }

    /// The row the operation read, when the checker closed it.
    pub fn input_row(&self) -> Option<&Row> {
        self.node.input_rows.first().and_then(Option::as_ref)
    }

    /// Rows read from each relational child, in the same order as `inputs`.
    pub fn input_rows(&self) -> impl Iterator<Item = Option<&Row>> {
        self.node.input_rows.iter().map(Option::as_ref)
    }

    /// The row the operation produced, when the checker closed it.
    pub fn output_row(&self) -> Option<&Row> {
        self.node.output_row.as_ref()
    }

    /// Where the operation was written.
    pub fn origin(&self) -> Origin {
        self.node.origin
    }

    /// The checked predicate carried by a `where` node.
    pub fn predicate(&self) -> Option<&CheckedExpr> {
        self.node.predicate.as_ref()
    }

    /// The checked sort keys owned by an `order` node, in source order.
    pub fn order_keys(&self) -> Option<&[(CheckedExpr, bool)]> {
        self.node.order_keys.as_deref()
    }

    /// The checked output fields owned by `select`, `update`, or `agg`.
    pub fn projection_fields(&self) -> Option<&[(String, CheckedExpr)]> {
        self.node.projection_fields.as_deref()
    }

    /// The checked `on` expression owned by a join node.
    pub fn join_predicate(&self) -> Option<&CheckedExpr> {
        self.node.join_predicate.as_ref()
    }
}

/// A definition's query as a typed graph.
///
/// The arena owns every node; a [`NodeId`] indexes it. The [`TypedQuery::root`]
/// is the node the definition's body produced, and a query's stages are reached
/// by following `input` links from it, so the chain is the structure rather than
/// a side list that has to be kept in the same order.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedQuery {
    nodes: Vec<TypedQueryNode>,
    root: NodeId,
}

/// What building a typed query node was refused for.
///
/// A compiler bug rather than a program error: every rejection below is a
/// constructor disagreeing with its own kind or its own child, which no source
/// program can cause. It is returned rather than asserted so a slice under
/// construction reports what it could not build, and it becomes an assertion at
/// the boundary where the route is total.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueryErr {
    pub message: String,
    pub origin: Origin,
    pub site: NodeRef,
}

impl QueryErr {
    fn new(node: &TypedQueryNode, message: impl Into<String>) -> Self {
        QueryErr {
            message: message.into(),
            origin: node.origin,
            site: node.site,
        }
    }
}

impl TypedQuery {
    /// An empty arena, ready for nodes.
    pub(crate) fn new() -> Self {
        TypedQuery {
            nodes: Vec::new(),
            root: NodeId(0),
        }
    }

    /// The node the definition's body produced.
    ///
    /// # Panics
    ///
    /// Only on a query with no nodes, which cannot be reached from outside this
    /// module: every constructor below produces at least one node, and the
    /// builder sets a root before returning the query.
    pub fn root(&self) -> NodeId {
        assert!(
            !self.nodes.is_empty(),
            "a typed query always has at least one node"
        );
        self.root
    }

    /// The node with this id.
    ///
    /// # Panics
    ///
    /// Only for an id from a different arena, which cannot be produced by this
    /// module: a [`NodeId`] is opaque, and the only way to get one is from the
    /// constructors and [`TypedQuery::chain`] of the same query.
    pub fn node(&self, id: NodeId) -> QueryNode<'_> {
        QueryNode {
            id,
            node: &self.nodes[id.index()],
        }
    }

    /// Every node, in creation order.
    ///
    /// Creation order is **not** the chain's order — see [`TypedQuery::chain`]
    /// for that — so this is for a consumer that wants the whole arena rather
    /// than the pipeline.
    pub fn nodes(&self) -> impl Iterator<Item = QueryNode<'_>> {
        self.nodes
            .iter()
            .enumerate()
            .map(move |(i, node)| QueryNode {
                id: NodeId::at(i),
                node,
            })
    }

    /// The stages of this query, in **application order**: the root first, then
    /// each node's input.
    ///
    /// That is the order the checker recorded a chain in and the order the SQL
    /// reads as outermost-first, so a consumer comparing against the checker's
    /// record walks this and gets the same sequence. Following `input` links
    /// rather than iterating the arena is deliberate: the arena is in creation
    /// order, which is *not* the chain's order once a node's input is built
    /// after its own children in a different order.
    pub fn chain(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut at = Some(self.root());
        while let Some(id) = at {
            out.push(id);
            at = self.raw(id).inputs.first().copied();
        }
        out
    }

    /// The kind of the node the body produced.
    pub fn root_kind(&self) -> QueryNodeKind {
        self.raw(self.root()).kind
    }

    /// The output row of the node the body produced.
    pub fn root_row(&self) -> Option<&Row> {
        self.raw(self.root()).output_row.as_ref()
    }

    /// Copy checked expression payloads onto the structurally corresponding
    /// typed nodes. This keeps payload ownership independent of whether the
    /// relational skeleton came from a direct pipeline or helper expansion.
    pub(crate) fn carry_checked_payloads(
        &mut self,
        checked: &crate::checked::CheckedQuery,
    ) -> Result<(), QueryErr> {
        fn visit(
            nodes: &mut [TypedQueryNode],
            id: NodeId,
            checked: &crate::checked::CheckedQuery,
        ) -> Result<(), QueryErr> {
            let typed_kind = nodes[id.index()].kind;
            let checked_kind = TypedQuery::checked_node_kind(checked.node_ref());
            if typed_kind != checked_kind {
                match checked.node_ref() {
                    // Payload-free checked wrappers may be absorbed by the
                    // typed skeleton. Walk through them to align descendants.
                    crate::checked::CheckedQueryNode::Distinct(input) => {
                        return visit(nodes, id, input);
                    }
                    // Some helper expansions currently omit the combine node
                    // from the typed skeleton. Keep hydrating any child whose
                    // operation is present, and leave the absent combine's
                    // payload unset.
                    crate::checked::CheckedQueryNode::Join { left, right, .. }
                    | crate::checked::CheckedQueryNode::Set { left, right, .. } => {
                        let matches: Vec<_> = [left.as_ref(), right.as_ref()]
                            .into_iter()
                            .filter(|child| {
                                TypedQuery::checked_node_kind(child.node_ref()) == typed_kind
                            })
                            .collect();
                        if let [child] = matches.as_slice() {
                            return visit(nodes, id, child);
                        }
                        return Err(QueryErr::new(
                            &nodes[id.index()],
                            format!(
                                "typed {:?} node does not align with checked {:?}",
                                typed_kind,
                                TypedQuery::checked_node_kind(checked.node_ref())
                            ),
                        ));
                    }
                    _ => {
                        return Err(QueryErr::new(
                            &nodes[id.index()],
                            format!(
                                "typed {:?} node does not align with checked {:?}",
                                typed_kind, checked_kind
                            ),
                        ));
                    }
                }
            }
            let (expected_kind, child_ids, payload) = match checked.node_ref() {
                crate::checked::CheckedQueryNode::Table { .. } => {
                    (QueryNodeKind::Table, Vec::new(), Payload::None)
                }
                crate::checked::CheckedQueryNode::Where { pred, .. } => (
                    QueryNodeKind::Where,
                    vec![id_at(nodes, id, 0)?],
                    Payload::Predicate(pred.clone()),
                ),
                crate::checked::CheckedQueryNode::Select { fields, .. } => (
                    QueryNodeKind::Select,
                    vec![id_at(nodes, id, 0)?],
                    Payload::Projection(fields.clone()),
                ),
                crate::checked::CheckedQueryNode::Update { fields, .. } => (
                    QueryNodeKind::Update,
                    vec![id_at(nodes, id, 0)?],
                    Payload::Projection(fields.clone()),
                ),
                crate::checked::CheckedQueryNode::Agg { fields, .. } => (
                    QueryNodeKind::Agg,
                    vec![id_at(nodes, id, 0)?],
                    Payload::Projection(fields.clone()),
                ),
                crate::checked::CheckedQueryNode::Order { keys, .. } => (
                    QueryNodeKind::Order,
                    vec![id_at(nodes, id, 0)?],
                    Payload::Order(keys.clone()),
                ),
                crate::checked::CheckedQueryNode::Join { kind, on, .. } => (
                    QueryNodeKind::Join(*kind),
                    ids_at(nodes, id, 2)?,
                    Payload::Join(on.clone()),
                ),
                crate::checked::CheckedQueryNode::Set { kind, .. } => (
                    QueryNodeKind::Set(*kind),
                    ids_at(nodes, id, 2)?,
                    Payload::None,
                ),
                node => {
                    let kind = match node {
                        crate::checked::CheckedQueryNode::Omit { .. } => QueryNodeKind::Omit,
                        crate::checked::CheckedQueryNode::Rename { prefix, .. } => {
                            if *prefix {
                                QueryNodeKind::Prefix
                            } else {
                                QueryNodeKind::Suffix
                            }
                        }
                        crate::checked::CheckedQueryNode::MapValue { .. } => {
                            QueryNodeKind::MapValue
                        }
                        crate::checked::CheckedQueryNode::Limit { .. } => QueryNodeKind::Limit,
                        crate::checked::CheckedQueryNode::Offset { .. } => QueryNodeKind::Offset,
                        crate::checked::CheckedQueryNode::Distinct(_) => QueryNodeKind::Distinct,
                        _ => unreachable!("handled checked query node above"),
                    };
                    (kind, vec![id_at(nodes, id, 0)?], Payload::None)
                }
            };
            let node = &mut nodes[id.index()];
            if node.kind != expected_kind {
                return Err(QueryErr::new(
                    node,
                    format!(
                        "typed {:?} node does not match checked {expected_kind:?}",
                        node.kind
                    ),
                ));
            }
            match payload {
                Payload::None => {}
                Payload::Predicate(value) => node.predicate = Some(value),
                Payload::Order(value) => node.order_keys = Some(value),
                Payload::Projection(value) => node.projection_fields = Some(value),
                Payload::Join(value) => node.join_predicate = Some(value),
            }
            let checked_children: Vec<_> = match checked.node_ref() {
                crate::checked::CheckedQueryNode::Table { .. } => Vec::new(),
                crate::checked::CheckedQueryNode::Where { input, .. }
                | crate::checked::CheckedQueryNode::Select { input, .. }
                | crate::checked::CheckedQueryNode::Update { input, .. }
                | crate::checked::CheckedQueryNode::Omit { input, .. }
                | crate::checked::CheckedQueryNode::Rename { input, .. }
                | crate::checked::CheckedQueryNode::MapValue { input, .. }
                | crate::checked::CheckedQueryNode::Agg { input, .. }
                | crate::checked::CheckedQueryNode::Order { input, .. }
                | crate::checked::CheckedQueryNode::Limit { input, .. }
                | crate::checked::CheckedQueryNode::Offset { input, .. } => {
                    one_child(nodes, id)?;
                    vec![input.as_ref()]
                }
                crate::checked::CheckedQueryNode::Distinct(input) => {
                    one_child(nodes, id)?;
                    vec![input.as_ref()]
                }
                crate::checked::CheckedQueryNode::Join { left, right, .. }
                | crate::checked::CheckedQueryNode::Set { left, right, .. } => {
                    vec![left.as_ref(), right.as_ref()]
                }
            };
            for (child_id, child) in child_ids.into_iter().zip(checked_children) {
                visit(nodes, child_id, child)?;
            }
            Ok(())
        }

        enum Payload {
            None,
            Predicate(CheckedExpr),
            Order(Vec<(CheckedExpr, bool)>),
            Projection(Vec<(String, CheckedExpr)>),
            Join(CheckedExpr),
        }
        fn id_at(nodes: &[TypedQueryNode], id: NodeId, index: usize) -> Result<NodeId, QueryErr> {
            nodes[id.index()].inputs.get(index).copied().ok_or_else(|| {
                QueryErr::new(&nodes[id.index()], "typed node is missing a checked child")
            })
        }
        fn ids_at(
            nodes: &[TypedQueryNode],
            id: NodeId,
            count: usize,
        ) -> Result<Vec<NodeId>, QueryErr> {
            let inputs = &nodes[id.index()].inputs;
            if inputs.len() != count {
                return Err(QueryErr::new(
                    &nodes[id.index()],
                    format!(
                        "typed {:?} node has {} children but checked tree expects {count}",
                        nodes[id.index()].kind,
                        inputs.len()
                    ),
                ));
            }
            Ok(inputs.clone())
        }
        fn one_child(nodes: &[TypedQueryNode], id: NodeId) -> Result<NodeId, QueryErr> {
            let inputs = &nodes[id.index()].inputs;
            if inputs.len() != 1 {
                return Err(QueryErr::new(
                    &nodes[id.index()],
                    format!(
                        "typed {:?} node has {} children but checked tree expects 1",
                        nodes[id.index()].kind,
                        inputs.len()
                    ),
                ));
            }
            Ok(inputs[0])
        }

        let root = self.root();
        visit(&mut self.nodes, root, checked)
    }

    /// Verify that every typed node which owns an expression has received its
    /// checked payload. This is the boundary that lets later lowering consume
    /// the typed graph without silently falling back to normalized terms.
    pub(crate) fn require_checked_payloads(&self) -> Result<(), QueryErr> {
        for node in &self.nodes {
            let missing = match node.kind {
                QueryNodeKind::Where => node.predicate.is_none(),
                QueryNodeKind::Order => node.order_keys.is_none(),
                QueryNodeKind::Select | QueryNodeKind::Update | QueryNodeKind::Agg => {
                    node.projection_fields.is_none()
                }
                QueryNodeKind::Join(_) => node.join_predicate.is_none(),
                _ => false,
            };
            if missing {
                return Err(QueryErr::new(
                    node,
                    format!("typed {:?} node is missing its checked payload", node.kind),
                ));
            }
        }
        Ok(())
    }

    /// Rebuild a checked relation from this typed graph when the graph and the
    /// existing checked tree have the same shape. The checked tree is used as
    /// the source of table metadata and literal stage arguments; expression
    /// payloads come from the typed nodes. A structural mismatch returns
    /// `None`, allowing callers to keep the normalized route as a comparison
    /// oracle for slices that have not reached one-to-one alignment yet.
    pub(crate) fn rebuild_checked(
        &self,
        template: &crate::checked::CheckedQuery,
    ) -> Result<Option<crate::checked::CheckedQuery>, QueryErr> {
        fn build(
            query: &TypedQuery,
            id: NodeId,
            template: &crate::checked::CheckedQuery,
        ) -> Result<Option<crate::checked::CheckedQuery>, QueryErr> {
            let node = &query.nodes[id.index()];
            let one = || {
                if node.inputs.len() != 1 {
                    return Err(QueryErr::new(
                        node,
                        format!(
                            "typed {:?} node has {} children but checked tree expects 1",
                            node.kind,
                            node.inputs.len()
                        ),
                    ));
                }
                Ok(node.inputs[0])
            };
            let two = || {
                if node.inputs.len() != 2 {
                    return Err(QueryErr::new(
                        node,
                        format!(
                            "typed {:?} node has {} children but checked tree expects 2",
                            node.kind,
                            node.inputs.len()
                        ),
                    ));
                }
                Ok((node.inputs[0], node.inputs[1]))
            };
            let built = match (node.kind, template.node_ref()) {
                (QueryNodeKind::Table, crate::checked::CheckedQueryNode::Table { .. }) => {
                    Some(template.clone())
                }
                (QueryNodeKind::Where, crate::checked::CheckedQueryNode::Where { input, .. }) => {
                    let Some(predicate) = node.predicate.clone() else {
                        return Ok(None);
                    };
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::where_(input, predicate, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Select, crate::checked::CheckedQueryNode::Select { input, .. }) => {
                    let Some(fields) = node.projection_fields.clone() else {
                        return Ok(None);
                    };
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::select(input, fields, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Update, crate::checked::CheckedQueryNode::Update { input, .. }) => {
                    let Some(fields) = node.projection_fields.clone() else {
                        return Ok(None);
                    };
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::update(input, fields, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Agg, crate::checked::CheckedQueryNode::Agg { input, .. }) => {
                    let Some(fields) = node.projection_fields.clone() else {
                        return Ok(None);
                    };
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::agg(input, fields, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Order, crate::checked::CheckedQueryNode::Order { input, .. }) => {
                    let Some(keys) = node.order_keys.clone() else {
                        return Ok(None);
                    };
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::order(input, keys, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Omit, crate::checked::CheckedQueryNode::Omit { input, key }) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::omit(input, key.clone(), template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (
                    QueryNodeKind::Prefix,
                    crate::checked::CheckedQueryNode::Rename {
                        input,
                        affix,
                        prefix: true,
                    },
                )
                | (
                    QueryNodeKind::Suffix,
                    crate::checked::CheckedQueryNode::Rename {
                        input,
                        affix,
                        prefix: false,
                    },
                ) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::rename(
                            input,
                            affix.clone(),
                            node.kind == QueryNodeKind::Prefix,
                            template.origin(),
                        )
                        .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (
                    QueryNodeKind::MapValue,
                    crate::checked::CheckedQueryNode::MapValue { input, wrapper },
                ) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::map_value(
                            input,
                            wrapper.clone(),
                            template.origin(),
                        )
                        .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Limit, crate::checked::CheckedQueryNode::Limit { input, n }) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::limit(input, *n, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Offset, crate::checked::CheckedQueryNode::Offset { input, n }) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::offset(input, *n, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (QueryNodeKind::Distinct, crate::checked::CheckedQueryNode::Distinct(input)) => {
                    let Some(input) = build(query, one()?, input)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::distinct(input, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (
                    QueryNodeKind::Join(kind),
                    crate::checked::CheckedQueryNode::Join {
                        kind: template_kind,
                        left,
                        right,
                        ..
                    },
                ) if kind == *template_kind => {
                    let Some(predicate) = node.join_predicate.clone() else {
                        return Ok(None);
                    };
                    let (left_id, right_id) = two()?;
                    let Some(left) = build(query, left_id, left)? else {
                        return Ok(None);
                    };
                    let Some(right) = build(query, right_id, right)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::join(
                            kind,
                            left,
                            right,
                            predicate,
                            template.origin(),
                        )
                        .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                (
                    QueryNodeKind::Set(kind),
                    crate::checked::CheckedQueryNode::Set {
                        kind: template_kind,
                        left,
                        right,
                        ..
                    },
                ) if kind == *template_kind => {
                    let (left_id, right_id) = two()?;
                    let Some(left) = build(query, left_id, left)? else {
                        return Ok(None);
                    };
                    let Some(right) = build(query, right_id, right)? else {
                        return Ok(None);
                    };
                    Some(
                        crate::checked::CheckedQuery::set(kind, left, right, template.origin())
                            .map_err(|error| QueryErr::new(node, error.message))?,
                    )
                }
                _ => return Ok(None),
            };
            let Some(built) = built else {
                return Ok(None);
            };
            if node
                .output_row
                .as_ref()
                .is_some_and(|row| row.columns() != built.row().columns())
            {
                return Ok(None);
            }
            Ok(Some(built))
        }

        build(self, self.root(), template)
    }

    fn checked_node_kind(node: &crate::checked::CheckedQueryNode) -> QueryNodeKind {
        match node {
            crate::checked::CheckedQueryNode::Table { .. } => QueryNodeKind::Table,
            crate::checked::CheckedQueryNode::Where { .. } => QueryNodeKind::Where,
            crate::checked::CheckedQueryNode::Select { .. } => QueryNodeKind::Select,
            crate::checked::CheckedQueryNode::Update { .. } => QueryNodeKind::Update,
            crate::checked::CheckedQueryNode::Omit { .. } => QueryNodeKind::Omit,
            crate::checked::CheckedQueryNode::Rename { prefix, .. } => {
                if *prefix {
                    QueryNodeKind::Prefix
                } else {
                    QueryNodeKind::Suffix
                }
            }
            crate::checked::CheckedQueryNode::MapValue { .. } => QueryNodeKind::MapValue,
            crate::checked::CheckedQueryNode::Agg { .. } => QueryNodeKind::Agg,
            crate::checked::CheckedQueryNode::Order { .. } => QueryNodeKind::Order,
            crate::checked::CheckedQueryNode::Limit { .. } => QueryNodeKind::Limit,
            crate::checked::CheckedQueryNode::Offset { .. } => QueryNodeKind::Offset,
            crate::checked::CheckedQueryNode::Distinct(_) => QueryNodeKind::Distinct,
            crate::checked::CheckedQueryNode::Join { kind, .. } => QueryNodeKind::Join(*kind),
            crate::checked::CheckedQueryNode::Set { kind, .. } => QueryNodeKind::Set(*kind),
        }
    }

    /// Add a node, returning its id.
    fn push(&mut self, node: TypedQueryNode) -> NodeId {
        let id = NodeId::at(self.nodes.len());
        self.nodes.push(node);
        id
    }

    /// The raw node, for this module's own use: the constructors read a child's
    /// fields directly, and the public view borrows the arena through
    /// [`TypedQuery::node`].
    fn raw(&self, id: NodeId) -> &TypedQueryNode {
        &self.nodes[id.index()]
    }

    /// Build a **leaf**: a table, with its columns known.
    ///
    /// The concrete-builder pattern every constructor here follows: it takes its
    /// child's id, looks the child up, checks what the kind requires of it, and
    /// pushes a node. Looking the child up is what makes "the node refers to
    /// typed children" a fact rather than a convention — an id from another
    /// arena is a lookup failure rather than a silently accepted index.
    pub(crate) fn table(
        &mut self,
        site: NodeRef,
        origin: Origin,
        columns: Vec<(String, ScalarType)>,
    ) -> Result<NodeId, QueryErr> {
        let node = TypedQueryNode {
            site,
            kind: QueryNodeKind::Table,
            inputs: Vec::new(),
            input_rows: Vec::new(),
            output_row: Some(Row::new(columns)),
            origin,
            predicate: None,
            order_keys: None,
            projection_fields: None,
            join_predicate: None,
        };
        Ok(self.push(node))
    }

    /// A **row rewrite** over one child: `prefix`, `suffix`, `omit`,
    /// `mapValue`.
    ///
    /// The output row is passed in rather than recomputed, because the checker
    /// already computed it: the operation's own signature instance produced it,
    /// and it is the same row `check::stage` asserts the operation reproduces
    /// from its input. Recomputing it here from the input row would be a second
    /// implementation of the row rule — the duplication this whole redesign
    /// exists to remove — so the constructor takes the checker's answer and only
    /// checks that the kind is one that rewrites.
    pub(crate) fn rewrite(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        input: NodeId,
        input_row: Option<Row>,
        output_row: Option<Row>,
        origin: Origin,
    ) -> Result<NodeId, QueryErr> {
        let child_row = self.raw(input).output_row.clone();
        let node = TypedQueryNode {
            site,
            kind,
            inputs: vec![input],
            input_rows: vec![input_row],
            output_row,
            origin,
            predicate: None,
            order_keys: None,
            projection_fields: None,
            join_predicate: None,
        };
        if !kind.recomputes_row() {
            return Err(QueryErr::new(
                &node,
                format!("{kind:?} is not a row rewrite and cannot be built as one"),
            ));
        }
        // A rewrite's input row is the child's output row, and the two are the
        // same signature instance's parameter and result. They are recorded
        // separately because each is a fact about *this* node, but a
        // disagreement means the child is not the input this stage read — a
        // compiler bug, and the one this link exists to catch.
        if let (Some(recorded), Some(child_row)) =
            (node.input_rows.first().and_then(Option::as_ref), &child_row)
        {
            if recorded != child_row {
                return Err(QueryErr::new(
                    &node,
                    format!(
                        "{kind:?} reads a row its input does not produce: \
                         stage read [{}], input produces [{}]",
                        recorded.names().join(", "),
                        child_row.names().join(", ")
                    ),
                ));
            }
        }
        Ok(self.push(node))
    }

    /// A **row-preserving** stage over one child: `where`, `order`, `limit`,
    /// `offset`, `distinct`.
    ///
    /// The output row is the input's, which is the whole content of the rule for
    /// these stages, so it is the one thing this constructor *does* derive —
    /// from a typed row that is already closed, not from a query type. Taking it
    /// from the child is exact rather than a guess: the checker's own typed
    /// stage record says the same, and typed lowering carries that boundary
    /// into this node.
    pub(crate) fn keep_row(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        input: NodeId,
        origin: Origin,
    ) -> Result<NodeId, QueryErr> {
        self.keep_row_with_predicate(site, kind, input, origin, None)
    }

    /// A row-preserving stage with an already checked predicate payload.
    pub(crate) fn keep_row_with_predicate(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        input: NodeId,
        origin: Origin,
        predicate: Option<CheckedExpr>,
    ) -> Result<NodeId, QueryErr> {
        self.keep_row_with_payload(site, kind, input, origin, predicate, None, None, None)
    }

    /// A row-preserving stage with checked expression payloads.
    #[allow(clippy::too_many_arguments)] // Compatibility payload adapter.
    pub(crate) fn keep_row_with_payload(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        input: NodeId,
        origin: Origin,
        predicate: Option<CheckedExpr>,
        order_keys: Option<Vec<(CheckedExpr, bool)>>,
        projection_fields: Option<Vec<(String, CheckedExpr)>>,
        join_predicate: Option<CheckedExpr>,
    ) -> Result<NodeId, QueryErr> {
        let row = self.raw(input).output_row.clone();
        let node = TypedQueryNode {
            site,
            kind,
            inputs: vec![input],
            input_rows: vec![row.clone()],
            output_row: row,
            origin,
            predicate,
            order_keys,
            projection_fields,
            join_predicate,
        };
        if kind.arity() != 1
            || kind.recomputes_row()
            || (node.predicate.is_some() && kind != QueryNodeKind::Where)
            || (node.order_keys.is_some() && kind != QueryNodeKind::Order)
            || node.projection_fields.is_some()
            || node.join_predicate.is_some()
        {
            return Err(QueryErr::new(
                &node,
                format!("{kind:?} does not preserve its input's row"),
            ));
        }
        Ok(self.push(node))
    }

    /// A **projection** over one child: `select`, `update`.
    ///
    /// A projection's output row is built from the expressions it was given, so
    /// it is *not* a function of the input and the checker's answer is passed in
    /// rather than derived. `input_row` is the child's, because a projection
    /// reads exactly its input's row — that is the fact the field expressions
    /// were checked against.
    #[allow(dead_code)]
    pub(crate) fn project(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        input: NodeId,
        output_row: Option<Row>,
        origin: Origin,
        projection_fields: Option<Vec<(String, CheckedExpr)>>,
    ) -> Result<NodeId, QueryErr> {
        let input_row = self.raw(input).output_row.clone();
        let node = TypedQueryNode {
            site,
            kind,
            inputs: vec![input],
            input_rows: vec![input_row],
            output_row,
            origin,
            predicate: None,
            order_keys: None,
            projection_fields,
            join_predicate: None,
        };
        if !matches!(
            kind,
            QueryNodeKind::Select | QueryNodeKind::Update | QueryNodeKind::Agg
        ) {
            return Err(QueryErr::new(
                &node,
                format!("{kind:?} is not a projection"),
            ));
        }
        Ok(self.push(node))
    }

    /// Combine two typed children with a join or set operation. The checker
    /// supplies the output row; this constructor records both child rows in
    /// operand order and refuses an operation of any other arity.
    #[allow(clippy::too_many_arguments)] // Compatibility payload adapter.
    pub(crate) fn combine(
        &mut self,
        site: NodeRef,
        kind: QueryNodeKind,
        left: NodeId,
        right: NodeId,
        output_row: Option<Row>,
        origin: Origin,
        join_predicate: Option<CheckedExpr>,
    ) -> Result<NodeId, QueryErr> {
        let node = TypedQueryNode {
            site,
            kind,
            inputs: vec![left, right],
            input_rows: vec![
                self.raw(left).output_row.clone(),
                self.raw(right).output_row.clone(),
            ],
            output_row,
            origin,
            predicate: None,
            order_keys: None,
            projection_fields: None,
            join_predicate,
        };
        if kind.arity() != 2 || !matches!(kind, QueryNodeKind::Join(_) | QueryNodeKind::Set(_)) {
            return Err(QueryErr::new(
                &node,
                format!("{kind:?} is not a two-input relational operation"),
            ));
        }
        if node.join_predicate.is_some() && !matches!(kind, QueryNodeKind::Join(_)) {
            return Err(QueryErr::new(
                &node,
                format!("{kind:?} cannot carry a join predicate"),
            ));
        }
        Ok(self.push(node))
    }

    /// Finish the query at `root`.
    pub(crate) fn finish(mut self, root: NodeId) -> TypedQuery {
        self.root = root;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checked::CheckedQuery;
    use cagara_syntax::ast::Span;

    fn site(module: usize, def: usize, expr_id: u32) -> NodeRef {
        NodeRef::new(module, def, expr_id)
    }

    fn origin() -> Origin {
        Origin::new(0, Span::default())
    }

    fn row(names: &[&str]) -> Row {
        Row::new(
            names
                .iter()
                .map(|n| (n.to_string(), ScalarType::Int))
                .collect(),
        )
    }

    /// A chain's order is the application order, root first — not the arena's
    /// creation order.
    #[test]
    fn the_chain_follows_input_links_not_creation_order() {
        let mut q = TypedQuery::new();
        // Deliberately created out of chain order: the leaf first, then the
        // stage above it.
        let base = q
            .table(site(0, 0, 1), origin(), row(&["id"]).columns().to_vec())
            .expect("a table");
        let named = q
            .rewrite(
                site(0, 0, 3),
                QueryNodeKind::Prefix,
                base,
                Some(row(&["id"])),
                Some(row(&["p_id"])),
                origin(),
            )
            .expect("a rename");
        let q = q.finish(named);

        let chain = q.chain();
        assert_eq!(chain.len(), 2);
        assert_eq!(q.node(chain[0]).kind(), QueryNodeKind::Prefix);
        assert_eq!(q.node(chain[1]).kind(), QueryNodeKind::Table);
        assert_eq!(q.root(), chain[0], "the root is the outermost stage");
    }

    /// A rewrite's recorded input row must be the row its child produces.
    ///
    /// This is the link that makes the `input` edge more than bookkeeping: it is
    /// how a node knows the child it was handed is the input it actually read.
    #[test]
    fn a_rewrite_rejects_an_input_that_produces_a_different_row() {
        let mut q = TypedQuery::new();
        let base = q
            .table(site(0, 0, 1), origin(), row(&["id"]).columns().to_vec())
            .expect("a table");
        // The child produces `id`, but the stage claims to have read `other`.
        let refused = q.rewrite(
            site(0, 0, 3),
            QueryNodeKind::Prefix,
            base,
            Some(row(&["other"])),
            Some(row(&["p_other"])),
            origin(),
        );
        assert!(refused.is_err(), "a mismatched input row is a compiler bug");
    }

    /// A kind that does not rewrite cannot be built as a rewrite, and one that
    /// does cannot be built as a row-preserving stage.
    ///
    /// The two constructors state the rule for their group, so a slice that
    /// reaches for the wrong one is refused instead of producing a node whose
    /// kind contradicts its rows.
    #[test]
    fn the_constructors_enforce_their_own_group() {
        let mut q = TypedQuery::new();
        let base = q
            .table(site(0, 0, 1), origin(), row(&["id"]).columns().to_vec())
            .expect("a table");
        assert!(
            q.rewrite(
                site(0, 0, 2),
                QueryNodeKind::Where,
                base,
                Some(row(&["id"])),
                Some(row(&["id"])),
                origin(),
            )
            .is_err(),
            "`where` is not a row rewrite"
        );
        assert!(
            q.keep_row(site(0, 0, 3), QueryNodeKind::Prefix, base, origin())
                .is_err(),
            "`prefix` is a rewrite, not a row-preserving stage"
        );
        assert!(
            q.project(
                site(0, 0, 4),
                QueryNodeKind::Table,
                base,
                None,
                origin(),
                None,
            )
            .is_err(),
            "a table is not a projection"
        );
    }

    /// A row-preserving stage takes its output row from its typed child.
    #[test]
    fn a_row_preserving_stage_reads_its_childs_row() {
        let mut q = TypedQuery::new();
        let base = q
            .table(
                site(0, 0, 1),
                origin(),
                row(&["id", "name"]).columns().to_vec(),
            )
            .expect("a table");
        let filtered = q
            .keep_row(site(0, 0, 2), QueryNodeKind::Where, base, origin())
            .expect("a filter");
        let q = q.finish(filtered);
        let node = q.node(q.root());
        assert_eq!(
            node.output_row().map(Row::names),
            Some(vec!["id".to_string(), "name".to_string()])
        );
        assert_eq!(node.input_row(), node.output_row());
    }

    /// `arity` states how many relational children each kind takes, and a table
    /// is the only leaf.
    #[test]
    fn arity_matches_the_kinds() {
        assert_eq!(QueryNodeKind::Table.arity(), 0);
        for kind in [
            QueryNodeKind::Prefix,
            QueryNodeKind::Suffix,
            QueryNodeKind::Omit,
            QueryNodeKind::MapValue,
            QueryNodeKind::Where,
            QueryNodeKind::Select,
            QueryNodeKind::Update,
            QueryNodeKind::Order,
            QueryNodeKind::Limit,
            QueryNodeKind::Offset,
            QueryNodeKind::Distinct,
        ] {
            assert_eq!(kind.arity(), 1, "{kind:?}");
        }
        assert_eq!(QueryNodeKind::Join(JoinKind::Inner).arity(), 2);
        assert_eq!(QueryNodeKind::Set(SetKind::Union).arity(), 2);
        assert!(QueryNodeKind::Prefix.recomputes_row());
        assert!(!QueryNodeKind::Where.recomputes_row());
    }

    /// Every fact a [`TypedQueryNode`] needs is available from the checker
    /// where the stage is applied.
    ///
    /// This is the feasibility claim behind the type, and it is asserted rather
    /// than assumed because the next slice depends on it entirely. For a
    /// `users & prefix "u_"` chain it checks, off the checker's own records,
    /// that:
    ///
    /// * the stage's identity is a [`NodeRef`] naming the application node — the
    ///   `site` a node is built with;
    /// * the rows it read and produced are both closed
    ///   (`Some(["id", "name"])` → `Some(["u_id", "u_name"])`), so
    ///   `input_row`/`output_row` have an answer rather than an open row;
    /// * the input query is a **typed child in the same definition's tree**
    ///   whose recorded row equals the stage's input row — which is what lets a
    ///   node link to a typed child by `ExprId` instead of to syntax, and is the
    ///   link [`TypedQuery::rewrite`] validates.
    #[test]
    fn the_checker_supplies_every_fact_a_typed_query_node_needs() {
        use crate::check::{RowStage, SchemeView};
        use crate::workspace::Workspace;

        let src = "users : query { id = int, name = string } = table \"s\" \"users\"\n\
                   q = users & prefix \"u_\"\n";
        let ws = Workspace::from_source(src);
        let tc = crate::check::check(&ws);
        assert!(tc.errors().is_empty(), "{:?}", tc.errors());
        let root = ws.root;
        let q = ws.modules[root]
            .module
            .defs
            .iter()
            .position(|d| d.name == "q")
            .expect("`q`");

        let stages: Vec<_> = tc.row_stages(root, q).collect();
        assert_eq!(stages.len(), 1, "one stage: {stages:?}");
        let stage = stages[0];
        assert_eq!(stage.kind, RowStage::Prefix);

        // The identity, and the rows, are all present.
        assert_eq!(stage.node.definition_module, root);
        assert_eq!(stage.node.definition, q);
        assert!(
            stage.input.is_some() && stage.output.is_some(),
            "a closed row on both sides: {stage:?}"
        );

        // The input is a typed child of the same definition, and its recorded
        // row is the row the stage read — which is what the node's `input` link
        // and `input_row` are, taken from the checker rather than recomputed.
        let body = tc.typed_body(root, q).expect("the definition's tree");
        let input_node = body
            .nodes()
            .iter()
            .find(|n| {
                matches!(n.view, Some(SchemeView::Query { .. }))
                    && n.id != stage.site
                    && n.row().as_ref() == stage.input.as_ref()
            })
            .expect("the input query is a typed node in the same tree");
        assert_eq!(
            input_node.row(),
            stage.input,
            "the typed child's row is the row the stage read"
        );
    }

    #[test]
    fn a_combine_records_both_children_and_rows_in_operand_order() {
        let mut q = TypedQuery::new();
        let left = q
            .table(site(0, 0, 1), origin(), row(&["left"]).columns().to_vec())
            .expect("left table");
        let right = q
            .table(site(0, 0, 2), origin(), row(&["right"]).columns().to_vec())
            .expect("right table");
        let join = q
            .combine(
                site(0, 0, 3),
                QueryNodeKind::Join(JoinKind::Inner),
                left,
                right,
                Some(row(&["left", "right"])),
                origin(),
                None,
            )
            .expect("two-input join");

        let node = q.node(join);
        assert_eq!(node.inputs(), &[left, right]);
        let input_rows: Vec<_> = node
            .input_rows()
            .map(|row| row.expect("table rows are closed").names())
            .collect();
        assert_eq!(input_rows, vec![vec!["left"], vec!["right"]]);
        assert_eq!(
            node.output_row().expect("join output row").names(),
            ["left", "right"]
        );

        let q = q.finish(join);
        // The legacy chain view follows the left operand; graph consumers use
        // `inputs()` to visit both children.
        assert_eq!(q.chain(), vec![join, left]);
    }

    #[test]
    fn combine_rejects_one_input_operations() {
        let mut q = TypedQuery::new();
        let left = q
            .table(site(0, 0, 1), origin(), row(&["id"]).columns().to_vec())
            .expect("left table");
        let right = q
            .table(site(0, 0, 2), origin(), row(&["id"]).columns().to_vec())
            .expect("right table");
        assert!(q
            .combine(
                site(0, 0, 3),
                QueryNodeKind::Where,
                left,
                right,
                Some(row(&["id"])),
                origin(),
                None,
            )
            .is_err());
    }

    #[test]
    fn checked_relation_can_be_rebuilt_from_typed_payloads() {
        let origin = origin();
        let columns = vec![("id".to_string(), ScalarType::Bool)];
        let checked_table = CheckedQuery::table(
            "s",
            "users",
            Some(crate::core::RowType::new(columns.clone())),
            origin,
        )
        .expect("checked table");
        let predicate = CheckedExpr::column(
            cagara_syntax::ast::Side::Single,
            "id",
            ScalarType::Bool,
            origin,
        );
        let checked =
            CheckedQuery::where_(checked_table, predicate.clone(), origin).expect("checked where");

        let mut typed = TypedQuery::new();
        let table = typed
            .table(site(0, 0, 1), origin, columns)
            .expect("typed table");
        let filtered = typed
            .keep_row_with_predicate(
                site(0, 0, 2),
                QueryNodeKind::Where,
                table,
                origin,
                Some(predicate),
            )
            .expect("typed where");
        let typed = typed.finish(filtered);
        let rebuilt = typed
            .rebuild_checked(&checked)
            .expect("rebuild succeeds")
            .expect("shapes align");
        assert_eq!(rebuilt.kind(), checked.kind());
        assert_eq!(rebuilt.row(), checked.row());
        assert_eq!(
            rebuilt.node_chain_predicates(),
            checked.node_chain_predicates()
        );
    }
}
