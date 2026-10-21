//! Static declarations for the language's built-in primitives.
//!
//! Primitive implementations belong to source elaboration. This module only
//! describes the names and type-level classification data used by resolution
//! and type checking.

use crate::ir::{JoinKind, SetKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Table,
    /// No stage spells this: `merge r s` is a row *type* former, and two queries
    /// are combined by a join or a set operation. The name stays in the table
    /// because `primitive "__merge"` is writable in a definition body, and the
    /// normalizer's answer to it is a compiler fault rather than silence.
    Merge,
    Where,
    Select,
    Update,
    Omit,
    MapValue,
    Prefix,
    Suffix,
    AggStage,
    Order,
    Limit,
    Offset,
    Distinct,
    In,
    Join(JoinKind),
    Set(SetKind),
    Group,
    Asc,
    Desc,
    Rows,
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRow,
    Preceding,
    Following,
}

/// The `__` vocabulary of `primitive "..."`.
///
/// This is the **only** place a primitive's name is spelled, and it is a table
/// the `primitive` body form reads — not a set of names in any module's scope.
/// `primitive "__where"` is like `sql "UPPER($1)"`: a built-in form whose
/// argument happens to be a word, resolved by the form rather than by import.
///
/// Keeping it out of scope is what makes the prelude an ordinary module. Its
/// `where = primitive "__where"` line is a definition like any other, and a
/// module that never imports the prelude simply has no `where`.
pub const PRIMS: &[(&str, Prim)] = &[
    ("__table", Prim::Table),
    ("__where", Prim::Where),
    ("__select", Prim::Select),
    ("__update", Prim::Update),
    ("__omit", Prim::Omit),
    ("__mapValue", Prim::MapValue),
    ("__merge", Prim::Merge),
    ("__prefix", Prim::Prefix),
    ("__suffix", Prim::Suffix),
    ("__agg", Prim::AggStage),
    ("__order", Prim::Order),
    ("__limit", Prim::Limit),
    ("__offset", Prim::Offset),
    ("__distinct", Prim::Distinct),
    ("__in", Prim::In),
    ("__innerJoin", Prim::Join(JoinKind::Inner)),
    ("__leftJoin", Prim::Join(JoinKind::Left)),
    ("__rightJoin", Prim::Join(JoinKind::Right)),
    ("__fullJoin", Prim::Join(JoinKind::Full)),
    ("__semiJoin", Prim::Join(JoinKind::Semi)),
    ("__antiJoin", Prim::Join(JoinKind::Anti)),
    ("__union", Prim::Set(SetKind::Union)),
    ("__unionAll", Prim::Set(SetKind::UnionAll)),
    ("__intersect", Prim::Set(SetKind::Intersect)),
    ("__except", Prim::Set(SetKind::Except)),
    ("__group", Prim::Group),
    ("__asc", Prim::Asc),
    ("__desc", Prim::Desc),
    ("__rows", Prim::Rows),
    ("__unboundedPreceding", Prim::UnboundedPreceding),
    ("__unboundedFollowing", Prim::UnboundedFollowing),
    ("__currentRow", Prim::CurrentRow),
    ("__preceding", Prim::Preceding),
    ("__following", Prim::Following),
];

/// The primitive `name` stands for, if it names one.
///
/// The single lookup the `primitive` body form uses. A second, partial copy of
/// this mapping used to live in the checker, so a primitive could be listed
/// here and still be rejected there; reading one table is the point.
pub fn prim_of(name: &str) -> Option<Prim> {
    PRIMS.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

impl Prim {
    /// Does this primitive's signature differ from its internal type?
    ///
    /// A projection takes a `row` of field expressions, which the checker
    /// carries separately from the surface `expr r (row s)` so the constraint
    /// solver can validate and lower the stage. Its signature therefore
    /// *describes* the internal type rather than being it, and the checker has
    /// to reconcile the two. Every other primitive's signature is its type.
    ///
    /// The set is declared here rather than as a second list of names in the
    /// checker, so a new projection cannot be added to one and forgotten in
    /// the other.
    pub fn is_projection(self) -> bool {
        use Prim::*;
        matches!(self, Where | Select | Update | AggStage | Join(_))
    }

    /// Does the compiler own this primitive's row rule?
    ///
    /// A projection's signature *describes* an internal type — the record of
    /// field expressions the stage constraint consumes — so `check_def`
    /// reconciles the two. A key stage's signature *is* its type, and the row
    /// rule that ties its input row to its output row is stated there
    /// (`removeKey k r` for `omit`, `keyMap m r` for the renames). Registering
    /// that rule once, for the definition, is what lets every use instantiate it
    /// with fresh variables instead of rebuilding it from the callee's result
    /// type. See `docs/CORE.md` for the reduction and row rules.
    pub fn owns_row_rule(self) -> bool {
        use Prim::*;
        matches!(self, Omit | Prefix | Suffix | MapValue)
    }

    pub fn arity(self) -> usize {
        use Prim::*;
        match self {
            UnboundedPreceding | UnboundedFollowing | CurrentRow => 0,
            Group | Asc | Desc | Preceding | Following => 1,
            Where | Select | Update | Omit | AggStage | Order | Limit | Offset | Table | Rows
            | Prefix | Suffix | Merge | MapValue => 2,
            Distinct => 1,
            In => 2,
            Join(_) => 3,
            Set(_) => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TplKind {
    Scalar,
    Agg,
    Win,
}
