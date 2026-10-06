// The checked core (`docs/CHECKED-CORE.md`): the result of type checking, the
// relational IR whose constructors enforce the semantic rules, and the
// intermediate term the evaluator produces. They are additive: `ir` and the
// SQL backend remain the compatibility layer underneath them.
pub mod check;
pub mod checked;
pub mod core;
pub mod core_term;
pub mod db;
pub mod eval;
pub mod ir;
pub mod lower;
pub mod prims;
pub mod resolve;
pub mod rules;
pub mod schema;
pub mod value;
pub mod workspace;

pub use check::{check, TypeCheck};
pub use checked::{
    erase, CheckedDef, CheckedExpr, CheckedExprNode, CheckedModule, CheckedProgram, CheckedQuery,
    CheckedQueryNode,
};
pub use core::{Diagnostic, Error, Origin, RowType, ScalarType};
// The evaluator's intermediate representation: relational primitives become
// named constructors here instead of being discovered dynamically through
// `Prim` (see `docs/CHECKED-CORE.md`).
pub use core_term::CoreTerm;
pub use db::{Database, SourceFile};
pub use eval::{root_queries, root_queries_checked, Evaluator};
pub use workspace::{Diag, Workspace};
