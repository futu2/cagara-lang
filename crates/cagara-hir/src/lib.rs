pub mod check;
pub mod db;
pub mod diagnostics;
pub mod eval;
pub mod ir;
pub mod lower;
pub mod resolve;
pub mod prims;
pub mod schema;
pub mod value;
pub mod workspace;

pub use check::{check, TypeCheck};
pub use db::{Database, SourceFile};
pub use diagnostics::Diagnostic;
pub use eval::{root_queries, root_queries_checked, Evaluator};
pub use lower::{lower_file, parse_module, ParsedModule};
pub use workspace::{Diag, Workspace};
