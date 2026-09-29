//! Editor features on top of the salsa-backed workspace: diagnostics,
//! hover (inferred types), and go-to-definition. The LSP transport is in
//! `main.rs`; everything here is plain functions, so it is unit-tested.

pub mod analysis;
pub mod uri;
