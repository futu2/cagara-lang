pub mod check;
pub mod checked;
pub mod core;
pub mod db;
pub mod ir;
pub mod lower;
pub mod primitive;
pub mod resolve;
pub mod rules;
pub mod schema;
pub mod workspace;

mod elaborate;
mod normalize;
pub use check::{check, probe_fields, TypeCheck};
pub use checked::{CheckedExpr, ExprKind};
pub use compile::{
    compile, compile_checked, compile_diagnostics, compile_input, root_queries,
    root_queries_checked, root_verdicts, Compilation, CompiledQuery, CompilerInput, DefinitionId,
    ModuleSnapshot,
};
pub use core::{Error, Fault, Origin, RowType, ScalarType};
pub use db::{Database, SourceFile};
pub use workspace::{Diag, Workspace};

mod compile;
mod incremental;
