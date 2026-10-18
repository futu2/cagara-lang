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
pub mod term_oracle;
pub mod workspace;

mod normalize;
mod query_lowering;
mod typed_lower;

pub use check::{check, TypeCheck};
// The checked core's public surface. `CheckedQuery`/`CheckedExpr` expose only
// accessors (`row()`, `phase()`, `ty()`, `kind()`, `origin()`); their node
// enums are deliberately *not* re-exported, so a caller outside this crate
// cannot assemble a value that skipped the constructors' checks.
pub use checked::{erase, CheckedExpr, CheckedQuery, ExprKind, QueryKind};
pub use compile::{
    compile, compile_checked, compile_diagnostics, compile_input, root_queries,
    root_queries_checked, root_verdicts, Compilation, CompiledQuery, CompilerInput, DefinitionId,
    DefinitionKey, ModuleSnapshot,
};
pub use core::{Diagnostic, Error, Fault, Origin, RowType, ScalarType};
pub use db::{Database, SourceFile};
// The checked tree of one root definition, for the pipeline snapshot test.
// `compile` reports erased relations, which is the public product; a change
// under this boundary is held to the checked tree as well, where node kinds,
// origins and rows are still visible. Not an interface to build on.
#[doc(hidden)]
pub use elaborate::elaborate_definition_for_tests;
/// The two query routes, as erased relations, for the differential test that
/// holds the normalizer to the lowering it replaces.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub use normalize::normalize_debug_for_tests;
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub use typed_lower::legacy_debug_for_tests;
pub use workspace::{Diag, Workspace};

mod compile;
mod incremental;
