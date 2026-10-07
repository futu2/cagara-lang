pub mod check;
pub mod checked;
pub mod core;
pub mod db;
pub mod elaborate;
pub mod ir;
pub mod lower;
pub mod primitive;
pub mod resolve;
pub mod rules;
pub mod schema;
pub mod workspace;

pub use check::{check, TypeCheck};
// The checked core's public surface. `CheckedQuery`/`CheckedExpr` expose only
// accessors (`row()`, `phase()`, `ty()`, `kind()`, `origin()`); their node
// enums are deliberately *not* re-exported, so a caller outside this crate
// cannot assemble a value that skipped the constructors' checks.
pub use checked::{erase, CheckedExpr, CheckedQuery, ExprKind, QueryKind};
pub use compile::{
    compile, compile_checked, compile_diagnostics, compile_input, root_queries,
    root_queries_checked, Compilation, CompiledQuery, CompilerInput, DefinitionId, DefinitionKey,
    ModuleSnapshot,
};
pub use core::{Diagnostic, Error, Origin, RowType, ScalarType};
pub use db::{Database, SourceFile};
pub use workspace::{Diag, Workspace};

mod compile;
