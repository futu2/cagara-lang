//! SQL backend: validated relational IR to SQL text via sqlglot-rust.

pub mod dialect;
mod emulate;
pub mod intrinsics;
pub mod lower;
pub mod rewrite;
pub mod stage;
mod visit;

pub use sqlglot_rust::Dialect;

use cagara_hir::ir::Rel;
use dialect::{support, Cap, Support};
use visit::{any_expr, any_rel, locate};

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

/// Compile `rel` for `opts.dialect`, with the message only.
///
/// A caller that has somewhere to put a position — `cagara check`, the CLI's error
/// report, the language server — should call [`compile_located`] instead: a lowering
/// refusal that names `0:0` points at the top of the file rather than at the stage
/// the reader has to change.
pub fn compile(rel: &Rel, opts: Options) -> Result<String, String> {
    compile_located(rel, opts).map_err(|(_, message)| message)
}

/// Compile `rel`, reporting **where** in the source the refusal came from.
///
/// The location is the innermost `Rel::At` the lowering had descended to when it
/// failed, or the node that a dialect check found. It is `None` when nothing in the
/// relation carries a span — an IR built by hand in a test, for instance — and a
/// caller has to say `0:0` then rather than inventing a line.
///
/// # The capability checks
///
/// Each check asks the internal capability table (`dialect::support`) what this target
/// can do with one construct, and the answer has three consequences:
///
/// * *native* or *unverified* — nothing to do; the lowerer writes the construct.
/// * *emulated* — the internal `emulate` pass rewrites it into equivalent SQL before
///   lowering, so the target is handed something it has and the query still means
///   what it says.
/// * *unsupported* — refuse, with a message that names the construct and what to
///   write instead.
///
/// `docs/SQL-DIALECTS.md` is the reader-facing version of that table.
///
/// Every check scans the **whole relation** through the internal `visit` walk, not just the stage
/// the construct happens to be written at. That is not a detail: the first version
/// of these checks inspected a `Select`'s or an `Agg`'s own expressions and stopped,
/// so `… & agg { k = rollup .k, n = count } & agg { total = count }` compiled for
/// SQLite and then failed at the server with `no such function: ROLLUP`. See
/// `visit`'s module documentation.
pub fn compile_located(
    rel: &Rel,
    opts: Options,
) -> Result<String, (Option<cagara_hir::ir::Loc>, String)> {
    cagara_hir::schema::schema(rel).map_err(|m| (None, m))?;

    // The window-frame refusals come first: they are about a clause the target's
    // grammar does not have, and the reader has to change the `frame` record.
    refuse_frame(rel, opts.dialect, Cap::GroupsFrame)?;
    refuse_frame(rel, opts.dialect, Cap::RangeFrame)?;
    // `unnest` and `list` values are refused by the intrinsic pass, which is where
    // their wording lives; the relational half of `unnest` is checked here because
    // it is a node rather than an expression.
    if any_rel(rel, &|r| matches!(r, Rel::Unnest(..))) {
        if let Some(msg) = intrinsics::unnest_refusal(opts.dialect) {
            return Err((locate(rel, &|r| matches!(r, Rel::Unnest(..))), msg));
        }
    }

    // The two constructs a target may lack but can still mean: rewrite them into
    // SQL every target reads, rather than refusing or emitting what the engine
    // rejects. This runs after validation (so a diagnostic names the query as
    // written) and before `rewrite` (so the rules that remove a derived table see
    // the shapes emulation produced).
    let emulated = emulate::emulate(rel, opts.dialect);
    let rel = &emulated;

    // The `Rel -> Rel` pass. `rewrite` has no dialect in it; see its module doc.
    let rewritten = rewrite::rewrite(rel);
    let rel = &rewritten;
    let mut lowerer = lower::Lowerer::for_rel(rel, opts.dialect);
    let st = lowerer.query(rel).map_err(|m| (lowerer.failed_at(), m))?;
    let at = lowerer.failed_at();
    let mut select = st.into_statement();
    select.ctes = lowerer.take_ctes();
    let mut stmt = sqlglot_rust::Statement::Select(select);
    if opts.optimize {
        stmt =
            sqlglot_rust::optimizer::optimize(stmt).map_err(|e| (at, format!("optimizer: {e}")))?;
    }
    let stmt = dialect::rewrite(stmt, opts.dialect).map_err(|e| (at, e))?;
    sqlglot_rust::validate_dialect_support(&stmt, opts.dialect)
        .map_err(|e| (at, format!("{e}")))?;
    let sql = if opts.pretty {
        sqlglot_rust::generate_pretty(&stmt, opts.dialect)
    } else {
        sqlglot_rust::generate(&stmt, opts.dialect)
    };
    // The one fixup that has to happen after generation: see `dialect::finalize`.
    Ok(dialect::finalize(sql, opts.dialect))
}

/// Refuse a window frame the target cannot read, naming the frame unit.
///
/// Both frame refusals are the same shape — "this target has no such unit" — and
/// both are about a *frame*, so the message is about what to write instead. A
/// frame has no equivalent spelling: `RANGE` answers the same as `GROUPS` only
/// when the order key has no peers within the offset, which is the case `groups`
/// exists to handle, and `ROWS` answers a different question in both.
fn refuse_frame(
    rel: &Rel,
    d: Dialect,
    cap: Cap,
) -> Result<(), (Option<cagara_hir::ir::Loc>, String)> {
    if support(cap, d).emits() {
        return Ok(());
    }
    let (unit, what) = match cap {
        Cap::GroupsFrame => (
            cagara_hir::ir::FrameUnit::Groups,
            "a `groups` window frame counts *peer groups*, and this target reads only \
             `ROWS` and `RANGE`. Use `rows` for a row count, or `range` when a distance \
             in the order key is what is meant",
        ),
        Cap::RangeFrame => (
            cagara_hir::ir::FrameUnit::Range,
            "a `range` window frame needs a single sort key, and this target's \
             `NULLS LAST` is emulated with a second one — MariaDB answers `RANGE-type \
             frame requires ORDER BY clause with single sort key`, and T-SQL's \
             equivalent refuses it too. Use `rows` for a row count, or `groups` on a \
             target that has it",
        ),
        _ => return Ok(()),
    };
    // A frame with no expression carrying it cannot be reported against a stage.
    if !any_expr(rel, &|e| frame_unit(e) == Some(unit)) {
        return Ok(());
    }
    // The location is the innermost stage the *frame* was written in — the node
    // whose own expressions carry it — rather than the root, which contains every
    // stage and so would match every time.
    let at = locate(rel, &|r| {
        visit::node_has_expr(r, &|e| frame_unit(e) == Some(unit))
    });
    Err((
        at,
        format!(
            "{what} — the target is `{}`, which {} this frame unit",
            dialect::name(d),
            if support(cap, d) == Support::Unsupported {
                "does not have"
            } else {
                "has not been checked for"
            }
        ),
    ))
}

/// The frame unit of a window expression, if it has a frame.
fn frame_unit(e: &cagara_hir::ir::Expr) -> Option<cagara_hir::ir::FrameUnit> {
    match e {
        cagara_hir::ir::Expr::Win(_, _, spec) => spec.frame.map(|f| f.unit),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
