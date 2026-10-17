//! The query lowering boundary.
//!
//! Query construction is owned by the checker-owned typed query graph. This
//! module keeps the boundary explicit: typed lowering returns its graph and a
//! checked query, while reporting whether the graph aligned with the checked
//! tree well enough to reconstruct its checked nodes.

use crate::check::TypedQuery;
use crate::checked::CheckedQuery;
use crate::compile::CompilerInput;
use crate::core::Error;
use cagara_syntax::ast::Expr;

/// The result of one query lowering route.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoweredQuery {
    /// A query built from checker-owned typed nodes. The typed graph is kept
    /// alongside the public checked tree so migration checks can inspect the
    /// stage boundaries without rebuilding them from the relation.
    Typed {
        typed: TypedQuery,
        checked: CheckedQuery,
        /// Whether the typed graph aligned with the checked tree well enough
        /// to reconstruct its nodes. Table metadata and literal arguments can
        /// still come from the checked adapter tree.
        rebuilt_checked_tree: bool,
    },
}

/// Lower one definition through the current query pipeline.
pub(crate) fn lower(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<LoweredQuery, Error> {
    if let Some((typed, checked, rebuilt_checked_tree)) =
        crate::typed_lower::lower_with_status(input, module, def, body)?
    {
        return Ok(LoweredQuery::Typed {
            typed,
            checked,
            rebuilt_checked_tree,
        });
    }

    Err(
        Error::unsupported("typed query lowering does not support this query shape")
            .at(crate::core::Origin::new(module, body.span)),
    )
}
