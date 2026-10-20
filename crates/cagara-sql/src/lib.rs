//! SQL backend: validated relational IR to SQL text via sqlglot-rust.

pub mod dialect;
pub mod intrinsics;
pub mod lower;
pub mod stage;

pub use sqlglot_rust::Dialect;

use cagara_hir::ir::{JoinKind, Rel};

/// Look up a dialect by name (`ansi`, `postgres`, `duckdb`, ...).
pub fn dialect(name: &str) -> Option<Dialect> {
    Dialect::from_str(name)
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dialect: Dialect,
    pub pretty: bool,
    /// Run sqlglot's optimizer. Off by default: it folds, simplifies and
    /// pushes predicates, so it is opt-in. The stage-boundary tests in
    /// `tests.rs` pin the cases Cagara relies on (window, LIMIT, aggregate).
    pub optimize: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dialect: Dialect::Ansi,
            pretty: false,
            optimize: false,
        }
    }
}

/// Validate and compile one query to SQL. The IR and `sql` templates are
/// ANSI; sqlglot then rewrites them for the target dialect (e.g. `||` →
/// `CONCAT` for MySQL / T-SQL, `LIMIT` → `TOP` / `FETCH`).
pub fn compile(rel: &Rel, opts: Options) -> Result<String, String> {
    cagara_hir::schema::schema(rel)?;
    if !dialect::supports_full_join(opts.dialect) && has_full_join(rel) {
        return Err(
            "`fullJoin` is not supported by the MySQL dialect family; rewrite it with \
             `leftJoin`/`rightJoin` and `unionAll`"
                .into(),
        );
    }
    let mut lowerer = lower::Lowerer::for_rel(rel);
    let st = lowerer.rel(rel)?;
    let mut select = st.into_statement();
    select.ctes = lowerer.take_ctes();
    let mut stmt = sqlglot_rust::Statement::Select(select);
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

/// Whether a relation contains a `fullJoin`, including under `At` wrappers.
fn has_full_join(rel: &Rel) -> bool {
    match rel.bare() {
        Rel::Join {
            kind: JoinKind::Full,
            ..
        } => true,
        rel => rel.children().into_iter().any(has_full_join),
    }
}

#[cfg(test)]
mod tests;
