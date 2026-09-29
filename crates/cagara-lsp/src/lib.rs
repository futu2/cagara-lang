//! Editor features on top of the salsa-backed workspace: diagnostics,
//! hover (inferred types), go-to-definition, references, highlights,
//! document symbols, and completion. The LSP transport is in `server.rs`
//! (started by `cagara lsp`); `analysis` is plain functions, so it is
//! unit-tested.

pub mod analysis;
pub mod server;
pub mod uri;

pub use server::run;
