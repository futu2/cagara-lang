//! The query lowering boundary.
//!
//! Query construction is migrating from the normalized term representation to
//! the checker-owned typed query graph. Keeping the route decision here makes
//! that migration explicit: typed lowering returns its graph and a checked
//! query, while reporting whether the graph aligned with the checked tree well
//! enough to reconstruct its checked nodes. If the typed builder declines the
//! whole shape, the legacy term adapter runs. A typed lowering error is never
//! reinterpreted as a request to try another semantic implementation.

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
    /// A query built by the temporary normalized-term adapter. This route is
    /// intentionally a distinct outcome so callers cannot mistake it for a
    /// successful typed lowering or silently add a second fallback.
    Legacy { checked: CheckedQuery },
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

    // Transitional adapter. It is the only place where the production query
    // boundary selects normalized lowering after the typed route declines a
    // shape; typed lowering never invokes it internally.
    let checked = crate::normalize::elaborate_query_via_terms(input, module, def, body)?;
    Ok(LoweredQuery::Legacy { checked })
}
