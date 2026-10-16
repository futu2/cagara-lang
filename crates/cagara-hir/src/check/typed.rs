//! A definition's body as a typed tree.
//!
//! The checker records facts per expression as it goes — the type a node got,
//! the row it is a query over, what a name resolved to — and hands them to
//! elaboration as maps keyed by `ExprId`. This is the same information attached
//! to the syntax it describes: one node per expression, children in source
//! order, so a consumer can walk a definition instead of walking its AST and
//! asking a map what it saw.
//!
//! The syntax is **not** copied. A node carries its `ExprId` and span; a
//! consumer that needs the shape reads the AST node it already has. The two
//! walks agree because both take their children from [`child_exprs`], which
//! states the order once.
//!
//! Facts live in one place — the node — and the indexes hold only integers, so
//! nothing here is a second copy of a fact. That is what let the maps go:
//! `use_tys`, `resolutions`, `choices`, `holes`, and `use_types` were each
//! deleted once its last reader read this instead. The two indexes are the
//! questions a reader arrives with: an `ExprId` (`by_id`) or the span a use was
//! filed under (`by_span`).

use super::child_exprs;
use super::*;
use cagara_syntax::ast;

/// One expression, with the facts the checker recorded for it.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedExpr {
    /// The node this describes.
    pub id: u32,
    /// Where it was written.
    pub span: Span,
    /// The span the checker filed this use under: the key of the published
    /// span map, and what a hover looks a type up by.
    ///
    /// Equal to `span` for every use in every program measured, and carried
    /// rather than assumed. The map is keyed by where a use was *recorded*, not
    /// by where it was written, and the tree is meant to answer the map's
    /// question — so it answers it under the map's key. A reader that used
    /// `span` and relied on the two agreeing would be trusting an observation
    /// instead of the key.
    pub key_span: Span,
    /// The checker's own classification of the type this node got, or `None`
    /// when it recorded none.
    ///
    /// Stored as the classification rather than as a `Ty`: a `Ty` never leaves
    /// the crate, and the questions a reader asks of it — what scalar, what
    /// row, which shape — are all projections of this, so storing it once is
    /// what keeps those readers from classifying a type differently.
    pub view: Option<SchemeView>,
    /// The printed type, as hover shows it for this node.
    pub shown: String,
    /// What a module-scope name occurrence resolved to, if it was one.
    pub resolved: Option<Resolved>,
    /// The overload choices recorded at this node, by hole index, ascending.
    ///
    /// A use site is an `ExprId`, so a choice belongs to the node the use was
    /// written at — which is why the map this replaces needed the site *and*
    /// the definition to be named before it could answer. Empty for the many
    /// nodes that are not a use site.
    pub choices: Vec<(usize, Choice)>,
    /// The query stage this node applied, if it is one.
    ///
    /// A stage is a *function value* — `prefix "u_"` has the type
    /// `query r -> query (keyMap m r)` — so applying one is an ordinary
    /// application. What makes it a stage is the callee's own scheme, and the
    /// facts recorded here are the checker's: the operation the definition
    /// declared, and the rows that stage's signature instance read and
    /// produced. They belong to the application node because that is where the
    /// operation was read and the signature instantiated.
    pub stage: Option<RowStageView>,
    /// Sub-expressions, as indices into the body's arena, in the order
    /// [`child_exprs`] states.
    children: Vec<usize>,
}

impl TypedExpr {
    /// The scalar type, as [`TypeCheck::use_ty`] reports it: `None` when the
    /// node got no type or a non-scalar one, `Some(Unknown)` when the checker
    /// never solved it.
    pub fn ty(&self) -> Option<crate::core::ScalarType> {
        match self.view.as_ref()? {
            SchemeView::Scalar(s) => Some(s.clone()),
            SchemeView::Open => Some(crate::core::ScalarType::Unknown),
            _ => None,
        }
    }

    /// The row, when the node is query-shaped.
    pub fn row(&self) -> Option<Vec<(String, crate::core::ScalarType)>> {
        match self.view.as_ref()? {
            SchemeView::Query { fields } => fields.clone(),
            _ => None,
        }
    }
}

/// A definition's body as a typed tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedBody {
    nodes: Vec<TypedExpr>,
    root: usize,
    /// How many overloads the definition leaves open to its users.
    ///
    /// A fact about the definition, not about a node: an open overload can be
    /// inherited through resolution and need not have a site of its own, which
    /// is why this cannot be counted from the nodes' choices.
    pub holes: usize,
    /// `ExprId` to arena index, ascending by id, so a lookup is a binary
    /// search rather than a second map of facts.
    by_id: Vec<(u32, usize)>,
    /// Key span to arena index, ascending by `(start, end)`, for a reader that
    /// arrives with the span a use was filed under.
    ///
    /// A repeated key span keeps the node the published map kept: the last use
    /// the checker recorded for it. The tie-break is resolved when the tree is
    /// built, while the recording order is still known, rather than by every
    /// reader.
    by_span: Vec<(u32, u32, usize)>,
    /// Stage applications by recording order: `(order, arena index)`.
    ///
    /// The fact is on the node; this is only where the chain's *order* lives.
    /// It has to be kept, because the two orders differ: a stage chain is
    /// recorded in application order, while the arena follows source order, and
    /// `users & prefix "u_" & suffix "_v2"` applies the prefix first but writes
    /// the suffix application outermost.
    by_stage: Vec<(usize, usize)>,
}

impl TypedBody {
    /// The body's own expression.
    pub fn root(&self) -> &TypedExpr {
        &self.nodes[self.root]
    }

    /// The node with this `ExprId`, if the definition has one.
    pub fn node(&self, id: u32) -> Option<&TypedExpr> {
        let at = self.by_id.binary_search_by_key(&id, |(id, _)| *id).ok()?;
        Some(&self.nodes[self.by_id[at].1])
    }

    /// The node the checker filed under this span, if it filed one.
    ///
    /// The span twin of [`TypedBody::node`]: the same question asked by
    /// location instead of by id.
    pub fn node_at(&self, span: Span) -> Option<&TypedExpr> {
        let at = self
            .by_span
            .binary_search_by_key(&(span.start, span.end), |(start, end, _)| {
                (*start, *end)
            })
            .ok()?;
        Some(&self.nodes[self.by_span[at].2])
    }

    /// Key spans and their arena indices, ascending, for a module-level index
    /// that holds positions rather than facts.
    pub(crate) fn span_index(&self) -> &[(u32, u32, usize)] {
        &self.by_span
    }

    /// The stage applications of this body, in application order.
    ///
    /// Adjacent nodes chain: a stage's input row is the previous stage's output
    /// row. Empty for a definition that applies no stage, and for one that
    /// failed — its stages were dropped when it did, so a failed definition
    /// never reports a chain it did not finish solving.
    pub fn stages(&self) -> impl Iterator<Item = &TypedExpr> {
        self.by_stage.iter().map(|&(_, at)| &self.nodes[at])
    }

    /// A node's sub-expressions, in source order.
    pub fn children<'a>(&'a self, node: &'a TypedExpr) -> impl Iterator<Item = &'a TypedExpr> {
        node.children.iter().map(|&i| &self.nodes[i])
    }

    /// Every node, in the order the walk built them.
    pub fn nodes(&self) -> &[TypedExpr] {
        &self.nodes
    }
}

/// What the checker recorded, as the lookups the builder makes by `ExprId`.
///
/// One struct rather than six parameters, because every field answers the same
/// kind of question — "what did the checker record at this node" — and the
/// builder's own inputs are only the body and the definition's open-overload
/// count. The `dyn` is deliberate: these are five closures of different types,
/// and each is called once per node, so naming them all in one signature would
/// cost more than the indirection does.
pub(crate) struct NodeFacts<'a> {
    /// The definition's stage applications, in application order.
    ///
    /// Each lands on the node whose `ExprId` it was recorded against, and the
    /// order is kept in an index beside `by_id`: the arena follows source order
    /// and a chain's order is not the same thing.
    pub(crate) stages: &'a [RowStageView],
    /// The type the node got, or `None` when the checker recorded none.
    pub(crate) view: &'a dyn Fn(u32) -> Option<SchemeView>,
    /// The printed type, as hover shows it.
    pub(crate) shown: &'a dyn Fn(u32) -> Option<String>,
    /// Where a use was filed and when it was recorded — `(key span, position in
    /// the checker's recording order)`. The order is what resolves a repeated
    /// key span the way the published map resolved it: the last write wins.
    pub(crate) recorded: &'a dyn Fn(u32) -> Option<(Span, usize)>,
    /// What a module-scope name occurrence resolved to.
    pub(crate) resolved: &'a dyn Fn(u32) -> Option<Resolved>,
    /// The overload choices recorded at the node, by hole.
    pub(crate) choices: &'a dyn Fn(u32) -> Vec<(usize, Choice)>,
}

/// Build a definition's typed tree.
///
/// Iterative, with an explicit stack: a deeply nested expression is exactly
/// where a recursive builder would overflow, and the checker's budgets exist
/// so that a program can be rejected rather than crash the compiler.
pub(crate) fn build(root_expr: &ast::Expr, holes: usize, facts: &NodeFacts<'_>) -> TypedBody {
    // The stage facts, by the node they were recorded at. A site is an
    // `ExprId`, so the node the operation was read at is the node that carries
    // it; the order travels separately.
    let mut stage_at: HashMap<u32, (usize, RowStageView)> = HashMap::new();
    for (order, stage) in facts.stages.iter().enumerate() {
        stage_at.insert(stage.site, (order, stage.clone()));
    }
    let mut nodes: Vec<TypedExpr> = Vec::new();
    let mut by_id: Vec<(u32, usize)> = Vec::new();
    let mut by_span: Vec<(u32, u32, usize, usize)> = Vec::new();
    let mut by_stage: Vec<(usize, usize)> = Vec::new();
    let mut root = 0usize;
    // `(expression, the node to attach it to)`.
    let mut stack: Vec<(&ast::Expr, Option<usize>)> = vec![(root_expr, None)];
    while let Some((e, parent)) = stack.pop() {
        let at = nodes.len();
        let recorded = (facts.recorded)(e.id);
        let stage = stage_at.get(&e.id);
        nodes.push(TypedExpr {
            id: e.id,
            span: e.span,
            key_span: recorded.map_or(e.span, |(span, _)| span),
            view: (facts.view)(e.id),
            shown: (facts.shown)(e.id).unwrap_or_default(),
            resolved: (facts.resolved)(e.id),
            choices: (facts.choices)(e.id),
            stage: stage.map(|(_, stage)| stage.clone()),
            children: Vec::new(),
        });
        by_id.push((e.id, at));
        if let Some((span, write)) = recorded {
            by_span.push((span.start, span.end, write, at));
        }
        if let Some((order, _)) = stage {
            by_stage.push((*order, at));
        }
        match parent {
            Some(p) => nodes[p].children.push(at),
            None => root = at,
        }
        // Reversed, so children are popped in source order and their arena
        // indices ascend in that order.
        for child in child_exprs(e).into_iter().rev() {
            stack.push((child, Some(at)));
        }
    }
    by_id.sort_unstable_by_key(|(id, _)| *id);
    // Ascending by key and, within one key, by recording order *descending*,
    // so the first of a repeated key is its last write. Dropping the rest is
    // what makes the index answer what the map answered.
    by_span.sort_unstable_by_key(|(start, end, write, _)| (*start, *end, std::cmp::Reverse(*write)));
    by_span.dedup_by_key(|(start, end, _, _)| (*start, *end));
    let by_span: Vec<(u32, u32, usize)> = by_span
        .into_iter()
        .map(|(start, end, _, at)| (start, end, at))
        .collect();
    by_stage.sort_unstable_by_key(|(order, _)| *order);
    TypedBody {
        nodes,
        root,
        by_id,
        by_span,
        by_stage,
        holes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_tree_matches_the_ast_and_answers_by_id() {
        let ws = Workspace::from_source("q = f 1 (g 2)\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        let tree = build(
            body,
            0,
            &NodeFacts {
                stages: &[],
                view: &|_| None,
                shown: &|_| None,
                recorded: &|_| None,
                resolved: &|_| None,
                choices: &|_| Vec::new(),
            },
        );

        // Every AST node is in the tree, exactly once, and the shapes agree.
        fn ids(e: &ast::Expr, out: &mut Vec<u32>) {
            out.push(e.id);
            for child in child_exprs(e) {
                ids(child, out);
            }
        }
        let mut expected = Vec::new();
        ids(body, &mut expected);
        let mut got: Vec<u32> = tree.nodes().iter().map(|n| n.id).collect();
        got.sort_unstable();
        expected.sort_unstable();
        assert_eq!(got, expected, "the tree covers the AST exactly");

        // A lookup by id finds the node, and the children are in source order.
        let root = tree.root();
        assert_eq!(root.id, body.id);
        let children: Vec<u32> = tree.children(root).map(|n| n.id).collect();
        let ast_children: Vec<u32> = child_exprs(body).into_iter().map(|e| e.id).collect();
        assert_eq!(children, ast_children);
        assert_eq!(tree.node(body.id).map(|n| n.id), Some(body.id));
        assert!(tree.node(u32::MAX).is_none(), "an unknown id finds nothing");
    }

    /// A key span the checker wrote twice answers with its last write.
    ///
    /// The published span map was a `HashMap`, so two uses filed under one span
    /// left the later recording in it. The tree replaces that map, so its span
    /// index has to break the tie the same way — and it can, because the
    /// recording order is still known while the tree is built.
    #[test]
    fn a_repeated_key_span_answers_with_its_last_write() {
        let ws = Workspace::from_source("q = f 1 2\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        let args = child_exprs(body);
        let (first, second) = (args[1].id, args[2].id);
        let shared = Span { start: 7, end: 8 };
        let tree = build(
            body,
            0,
            &NodeFacts {
                stages: &[],
                view: &|_| None,
                shown: &|id| Some(format!("t{id}")),
                recorded: &|id| match id {
                    id if id == first => Some((shared, 0)),
                    id if id == second => Some((shared, 1)),
                    _ => None,
                },
                resolved: &|_| None,
                choices: &|_| Vec::new(),
            },
        );

        assert_eq!(
            tree.node_at(shared).map(|n| n.id),
            Some(second),
            "the later recording is the one the map kept"
        );
        assert_eq!(
            tree.span_index().len(),
            1,
            "the repeated key is one entry, not two"
        );
        assert!(
            tree.node_at(body.span).is_none(),
            "a span nothing was filed under finds nothing"
        );
    }
}
