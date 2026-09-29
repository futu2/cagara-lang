//! SQL backend: validated relational IR to SQL text via sqlglot-rust.

pub mod dialect;
pub mod intrinsics;
pub mod lower;
pub mod stage;

pub use sqlglot_rust::Dialect;

use cagara_hir::ir::Rel;

/// Look up a dialect by name (`ansi`, `postgres`, `duckdb`, ...).
pub fn dialect(name: &str) -> Option<Dialect> {
    Dialect::from_str(name)
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dialect: Dialect,
    pub pretty: bool,
    /// Run sqlglot's optimizer. Off by default: its predicate pushdown can
    /// move a filter across a window or LIMIT boundary.
    pub optimize: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { dialect: Dialect::Ansi, pretty: false, optimize: false }
    }
}

/// Validate and compile one query to SQL. The IR and `sql` templates are
/// ANSI; sqlglot then rewrites them for the target dialect (e.g. `||` →
/// `CONCAT` for MySQL / T-SQL, `LIMIT` → `TOP` / `FETCH`).
pub fn compile(rel: &Rel, opts: Options) -> Result<String, String> {
    cagara_hir::schema::schema(rel)?;
    let st = lower::Lowerer::default().rel(rel)?;
    let mut stmt = sqlglot_rust::Statement::Select(st.into_statement());
    if opts.optimize {
        stmt = sqlglot_rust::optimizer::optimize(stmt).map_err(|e| format!("optimizer: {e}"))?;
    }
    let stmt = dialect::rewrite(stmt, opts.dialect)?;
    sqlglot_rust::validate_dialect_support(&stmt, opts.dialect).map_err(|e| format!("{e}"))?;
    Ok(if opts.pretty {
        sqlglot_rust::generate_pretty(&stmt, opts.dialect)
    } else {
        sqlglot_rust::generate(&stmt, opts.dialect)
    })
}

#[cfg(test)]
mod tests;
