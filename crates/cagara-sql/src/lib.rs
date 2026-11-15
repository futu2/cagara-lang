//! SQL backend: validated relational IR to SQL text via sqlglot-rust.

pub mod dialect;
pub mod intrinsics;
pub mod lower;
pub mod rewrite;
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
/// Does any window in this relation use a `RANGE` frame?
fn has_range_frame(rel: &Rel) -> bool {
    fn expr(e: &cagara_hir::ir::Expr) -> bool {
        e.any(&|e| match e {
            cagara_hir::ir::Expr::Win(_, _, spec) => spec
                .frame
                .is_some_and(|f| f.unit == cagara_hir::ir::FrameUnit::Range),
            _ => false,
        })
    }
    match rel {
        Rel::Select(_, fields) => fields.iter().any(|(_, e)| expr(e)),
        _ => rel.children().into_iter().any(has_range_frame),
    }
}

/// Does any window in this relation use a `GROUPS` frame?
///
/// The frame lives on an expression, not on the relation, so this walks the tree —
/// the same shape as `has_full_join` and `has_grouping_sets`, and for the same
/// reason: the rule is about what the statement contains, and only the completed
/// relation knows that.
fn has_groups_frame(rel: &Rel) -> bool {
    fn expr(e: &cagara_hir::ir::Expr) -> bool {
        e.any(&|e| match e {
            cagara_hir::ir::Expr::Win(_, _, spec) => spec
                .frame
                .is_some_and(|f| f.unit == cagara_hir::ir::FrameUnit::Groups),
            _ => false,
        })
    }
    match rel {
        Rel::Select(_, fields) => fields.iter().any(|(_, e)| expr(e)),
        _ => rel.children().into_iter().any(has_groups_frame),
    }
}

/// Does this relation unnest anywhere?
fn has_unnest(rel: &Rel) -> bool {
    matches!(rel, Rel::Unnest(..)) || rel.children().into_iter().any(has_unnest)
}

pub fn compile(rel: &Rel, opts: Options) -> Result<String, String> {
    cagara_hir::schema::schema(rel)?;
    if !dialect::supports_full_join(opts.dialect) && has_full_join(rel) {
        return Err(
            "`fullJoin` is not supported by the MySQL dialect family; rewrite it with \
             `leftJoin`/`rightJoin` and `unionAll`"
                .into(),
        );
    }
    if !dialect::supports_grouping_sets(opts.dialect) && has_grouping_sets(rel) {
        return Err(if dialect::fam(opts.dialect) == dialect::Fam::Mysql {
            "`rollup` and `cube` are not supported by the MySQL dialect family: its \
             `GROUP BY … WITH ROLLUP` rolls up the whole key list and cannot be \
             combined with `ORDER BY`, so it is not the same operation. Compute the \
             subtotals with a separate `agg` and `union` them in"
                .into()
        } else {
            "`rollup` and `cube` are not supported by the SQLite dialect, which has \
             no grouping sets"
                .into()
        });
    }
    // A `RANGE` frame needs exactly one sort key, and the MySQL family's `NULLS
    // LAST` emulation adds a second. So `range` and the emulation cannot both
    // happen: MariaDB answers `RANGE-type frame requires ORDER BY clause with single
    // sort key` (4019), measured. Refusing says which of the two to give up; writing
    // both would be SQL that runs nowhere.
    //
    // This is a narrower rule than "MySQL has no `RANGE`": it does, and it works with
    // a single key. What it cannot have is a *null-ordered* `range` frame, which is
    // every `range` frame this compiler writes, because a window's sort key is always
    // spelled `NULLS LAST`.
    if !dialect::supports_range_frame(opts.dialect) && has_range_frame(rel) {
        return Err(
            "a `range` window frame needs a single sort key, and this target's \
             `NULLS LAST` is emulated with a second one — MariaDB answers `RANGE-type \
             frame requires ORDER BY clause with single sort key`, and T-SQL's \
             equivalent refuses it too. Use `rows` for a row count, or `groups` on a \
             target that has it"
                .into(),
        );
    }
    if !dialect::supports_groups_frame(opts.dialect) && has_groups_frame(rel) {
        return Err(
            "a `groups` window frame is not supported by the MySQL dialect family: it \
             reads `ROWS` and `RANGE` and rejects `GROUPS`. Use `rows` for a row \
             count, or `range` when a distance in the order key is what is meant"
                .into(),
        );
    }
    // `unnest` is a relational node, so its refusal is here rather than in the
    // intrinsic pass: the lowerer has no dialect, and the wording belongs with
    // the other `list` refusals.
    if has_unnest(rel) {
        if let Some(msg) = intrinsics::unnest_refusal(dialect::fam(opts.dialect)) {
            return Err(msg);
        }
    }
    // The `Rel -> Rel` pass, after validation (so diagnostics name the query as
    // written) and before lowering (so the lowerer only ever sees stages it can
    // decide one at a time). `rewrite` has no dialect in it; see its module doc.
    let rewritten = rewrite::rewrite(rel);
    let rel = &rewritten;
    let mut lowerer = lower::Lowerer::for_rel(rel, dialect::supports_qualify(opts.dialect));
    let st = lowerer.rel(rel)?;
    let mut select = st.into_statement();
    select.ctes = lowerer.take_ctes();
    let mut stmt = sqlglot_rust::Statement::Select(select);
    if opts.optimize {
        stmt = sqlglot_rust::optimizer::optimize(stmt).map_err(|e| format!("optimizer: {e}"))?;
    }
    let stmt = dialect::rewrite(stmt, opts.dialect)?;
    sqlglot_rust::validate_dialect_support(&stmt, opts.dialect).map_err(|e| format!("{e}"))?;
    let sql = if opts.pretty {
        sqlglot_rust::generate_pretty(&stmt, opts.dialect)
    } else {
        sqlglot_rust::generate(&stmt, opts.dialect)
    };
    // The one fixup that has to happen after generation: see `dialect::finalize`.
    Ok(dialect::finalize(sql, opts.dialect))
}

/// Whether a relation asks for subtotal rows, anywhere in the tree.
fn has_grouping_sets(rel: &Rel) -> bool {
    match rel.bare() {
        Rel::Agg(_, fs) => fs
            .iter()
            .any(|(_, e)| e.any(&|x| matches!(x, cagara_hir::ir::Expr::Grouping(..)))),
        rel => rel.children().into_iter().any(has_grouping_sets),
    }
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
