pub mod check;
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
pub use db::{Database, SourceFile};
pub use eval::{root_queries, root_queries_checked, Evaluator};
pub use workspace::{Diag, Workspace};
