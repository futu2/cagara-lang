//! The production boundary between query lowering and the rest of the
//! compiler.
//!
//! One function: normalize a definition's body to an erased relation. The
//! intermediate value — the typed arena — exists only while this call runs, so
//! no consumer can acquire a half-built query, and the compiler's product is
//! the same `Result<Rel, Error>` it has always been.
//!
//! The classification is the one the pipeline has always made, now stated
//! once: a reduction that stops is a **compiler** fault when the normalizer had
//! no rule for an accepted construct, and a **program** fault when the rule
//! accepted the construct and refused what it was given (a table whose columns
//! are unknown, a `limit` with a negative count). The distinction is
//! [`Control`]'s, and [`Control::into_error`] is where it becomes a diagnostic.

use cagara_syntax::ast::Expr;

use super::arena::CompiledQuery;
use super::{Control, Lowered};
use crate::compile::CompilerInput;
use crate::core::Error;
use crate::ir::Rel;

/// Normalize one definition's body and erase the result.
pub(crate) fn lower(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Rel, Error> {
    query(input, module, def, body)?.erase()
}

/// Normalize one definition's body to the typed arena, for a caller that needs
/// the relational graph rather than the relation.
pub(crate) fn query(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<CompiledQuery, Error> {
    reduce(input, module, def, body).map(Lowered::into_query)
}

/// Normalize one definition's body, keeping the refusal's classification.
pub(crate) fn reduce(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Lowered, Error> {
    super::definition(input, module, def, body).map_err(Control::into_error)
}
