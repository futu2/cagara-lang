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
//! Facts live in one place — the node — and the id index holds only integers,
//! so nothing here is a second copy of a fact. That is the point: `use_tys` and
//! `resolutions` can go once their last reader reads this instead.

use super::child_exprs;
use super::*;
use cagara_syntax::ast;

/// The facts the checker recorded for one expression.
#[derive(Debug, Clone, PartialEq)]
struct NodeFacts {
    /// The scalar type, as [`TypeCheck::use_ty`] reports it: `None` when the
    /// node got no type or a non-scalar one, `Some(Unknown)` when the checker
    /// never solved it.
    ty: Option<crate::core::ScalarType>,
    /// The row, when the node is query-shaped.
    row: Option<Vec<(String, crate::core::ScalarType)>>,
    /// The printed type, as [`TypeCheck::use_type`] shows it for this node.
    shown: String,
    /// What a module-scope name occurrence resolved to, if it was one.
    resolved: Option<Resolved>,
}

impl NodeFacts {
    /// Read a node's facts off the type the checker resolved for it.
    ///
    /// The two answers `use_ty` and `use_row` give are one `SchemeView` apart,
    /// so they are computed together here rather than by two readers that could
    /// classify a type differently.
    fn of(ty: &Ty, shown: String, resolved: Option<Resolved>) -> NodeFacts {
        let (scalar, row) = match scheme_view(ty) {
            SchemeView::Scalar(s) => (Some(s), None),
            SchemeView::Open => (Some(crate::core::ScalarType::Unknown), None),
            SchemeView::Query { fields } => (None, fields),
            _ => (None, None),
        };
        NodeFacts {
            ty: scalar,
            row,
            shown,
            resolved,
        }
    }
}

/// One expression, with the facts the checker recorded for it.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedExpr {
    /// The node this describes.
    pub id: u32,
    /// Where it was written.
    pub span: Span,
    /// The scalar type, as [`TypeCheck::use_ty`] reports it.
    pub ty: Option<crate::core::ScalarType>,
    /// The row, when the node is query-shaped.
    pub row: Option<Vec<(String, crate::core::ScalarType)>>,
    /// The printed type, as hover shows it for this node.
    pub shown: String,
    /// What a module-scope name occurrence resolved to, if it was one.
    pub resolved: Option<Resolved>,
    /// Sub-expressions, as indices into the body's arena, in the order
    /// [`child_exprs`] states.
    children: Vec<usize>,
}

/// A definition's body as a typed tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedBody {
    nodes: Vec<TypedExpr>,
    root: usize,
    /// `ExprId` to arena index, ascending by id, so a lookup is a binary
    /// search rather than a second map of facts.
    by_id: Vec<(u32, usize)>,
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

    /// A node's sub-expressions, in source order.
    pub fn children<'a>(&'a self, node: &'a TypedExpr) -> impl Iterator<Item = &'a TypedExpr> {
        node.children.iter().map(|&i| &self.nodes[i])
    }

    /// Every node, in the order the walk built them.
    pub fn nodes(&self) -> &[TypedExpr] {
        &self.nodes
    }
}

/// Build a definition's typed tree.
///
/// Iterative, with an explicit stack: a deeply nested expression is exactly
/// where a recursive builder would overflow, and the checker's budgets exist
/// so that a program can be rejected rather than crash the compiler.
pub(crate) fn build(
    root_expr: &ast::Expr,
    ty_of: impl Fn(u32) -> Option<Ty>,
    shown_of: impl Fn(u32) -> Option<String>,
    resolved_of: impl Fn(u32) -> Option<Resolved>,
) -> TypedBody {
    let mut nodes: Vec<TypedExpr> = Vec::new();
    let mut by_id: Vec<(u32, usize)> = Vec::new();
    let mut root = 0usize;
    // `(expression, the node to attach it to)`.
    let mut stack: Vec<(&ast::Expr, Option<usize>)> = vec![(root_expr, None)];
    while let Some((e, parent)) = stack.pop() {
        let at = nodes.len();
        let facts = ty_of(e.id)
            .map(|t| NodeFacts::of(&t, shown_of(e.id).unwrap_or_default(), resolved_of(e.id)));
        nodes.push(TypedExpr {
            id: e.id,
            span: e.span,
            ty: facts.as_ref().and_then(|f| f.ty.clone()),
            row: facts.as_ref().and_then(|f| f.row.clone()),
            shown: facts.map(|f| f.shown).unwrap_or_default(),
            resolved: resolved_of(e.id),
            children: Vec::new(),
        });
        by_id.push((e.id, at));
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
    TypedBody { nodes, root, by_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_tree_matches_the_ast_and_answers_by_id() {
        let ws = Workspace::from_source("q = f 1 (g 2)\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        let tree = build(body, |_| None, |_| None, |_| None);

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
}
