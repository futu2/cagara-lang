//! SQL backend: validated relational IR to SQL text via sqlglot-rust.

pub mod lower;
pub mod stage;

pub use sqlglot_rust::Dialect;

use cagara_hir::ir::Rel;

/// Look up a dialect by name (`ansi`, `postgres`, `duckdb`, ...).
pub fn dialect(name: &str) -> Option<Dialect> {
    Dialect::from_str(name)
}

/// Validate and compile one query to SQL.
pub fn compile(rel: &Rel, dialect: Dialect, pretty: bool) -> Result<String, String> {
    cagara_hir::schema::schema(rel)?;
    let st = lower::Lowerer::default().rel(rel)?;
    let stmt = sqlglot_rust::Statement::Select(st.into_statement());
    Ok(if pretty {
        sqlglot_rust::generate_pretty(&stmt, dialect)
    } else {
        sqlglot_rust::generate(&stmt, dialect)
    })
}

#[cfg(test)]
mod tests;
