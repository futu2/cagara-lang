//! Static declarations for the language's built-in primitives.
//!
//! Primitive implementations belong to source elaboration. This module only
//! describes the names and type-level classification data used by resolution
//! and type checking.

use crate::ir::{JoinKind, SetKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Table,
    Where,
    Select,
    Update,
    Omit,
    MapValue,
    Merge,
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

impl Prim {
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
