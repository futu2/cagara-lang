//! Reduction of row terms (`keyMap`, `mapValue`, `merge`) plus the
//! constructor and kind tables that describe them.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

use crate::ir::Phase;

/// Reduce a MapKey term when the row and the mapper are both known.
/// Returns None if either is open (contains a variable) — the term is then
/// *retained*, which is what makes `prefix p` usable for a lambda parameter.
pub(crate) fn reduce_mapkey(km: &Ty, row: &Ty) -> Option<Ty> {
    // A variable mapper cannot be applied: there is no closed transformation
    // yet. The caller retains `keyMap m r` and tries again after binding.
    let km = KeyMap::of_ty(km)?;
    match row {
        Ty::Empty => Some(Ty::Empty),
        Ty::Row(fields, tail) => {
            // Check if tail is closed
            match tail.as_ref() {
                Ty::Empty => {
                    // Closed row: apply the mapper to each field name
                    let new_fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|(name, ty)| (km.apply(name), ty.clone()))
                        .collect();

                    // Validate: no empty names, no collisions
                    for (name, _) in &new_fields {
                        if name.is_empty() {
                            // Empty name error - for now, just defer (return None)
                            // The constraint solver will catch this properly
                            return None;
                        }
                    }

                    // Check for collisions
                    let mut seen = std::collections::HashSet::new();
                    for (name, _) in &new_fields {
                        if !seen.insert(name) {
                            // Collision detected - defer to constraint solver for proper error
                            return None;
                        }
                    }

                    Some(Ty::Row(new_fields, Box::new(Ty::Empty)))
                }
                Ty::Var(_) | Ty::Rigid(..) => None, // Open tail: defer
                Ty::MapKey(inner_km, inner_row) => {
                    // Nested MapKey: compose and reduce. `inner_km` runs first
                    // because it maps the row `inner_row` already produced.
                    let composed =
                        Ty::Con("KeyMapCompose", vec![inner_km.as_ref().clone(), km.to_ty()]);
                    reduce_mapkey(&composed, inner_row)
                }
                _ => None, // Unexpected tail shape
            }
        }
        Ty::Var(_) | Ty::Rigid(..) => None, // Open row: defer
        Ty::MapKey(inner_km, inner_row) => {
            // Nested MapKey: compose and try to reduce
            let composed = Ty::Con("KeyMapCompose", vec![inner_km.as_ref().clone(), km.to_ty()]);
            reduce_mapkey(&composed, inner_row)
        }
        _ => None, // Not a row
    }
}

/// Reduce a merge of two rows, if both are closed.
/// Implements the rule: merge old new = old fields with collisions replaced by
/// new, plus new fields not in old (appended at end).
pub(crate) fn reduce_merge(left: &Ty, right: &Ty) -> Option<Ty> {
    // Extract fields from both sides
    let (left_fields, left_tail) = match left {
        Ty::Empty => (vec![], Ty::Empty),
        Ty::Row(fields, tail) => (fields.clone(), tail.as_ref().clone()),
        Ty::Var(_) | Ty::Rigid(..) => return None, // Open: defer
        _ => return None,
    };

    let (right_fields, right_tail) = match right {
        Ty::Empty => (vec![], Ty::Empty),
        Ty::Row(fields, tail) => (fields.clone(), tail.as_ref().clone()),
        Ty::Var(_) | Ty::Rigid(..) => return None, // Open: defer
        _ => return None,
    };

    // Both tails must be Empty for a closed merge
    if !matches!(left_tail, Ty::Empty) || !matches!(right_tail, Ty::Empty) {
        return None;
    }

    // Right-wins merge: keep left field positions, replace collisions with right values,
    // append right fields not in left
    let mut result = left_fields.clone();

    // Replace collisions
    for (right_name, right_ty) in &right_fields {
        if let Some(pos) = result.iter().position(|(n, _)| n == right_name) {
            result[pos].1 = right_ty.clone();
        }
    }

    // Append right-only fields
    for (right_name, right_ty) in &right_fields {
        if !left_fields.iter().any(|(n, _)| n == right_name) {
            result.push((right_name.clone(), right_ty.clone()));
        }
    }

    Some(Ty::Row(result, Box::new(Ty::Empty)))
}

/// Reduce a MapValue term when the row is closed.
/// Returns None if the row is open (contains a variable tail).
pub(crate) fn reduce_mapvalue(mapper: &ValueMap, row: &Ty) -> Option<Ty> {
    match row {
        Ty::Empty => Some(Ty::Empty),
        Ty::Row(fields, tail) => {
            // If the tail is a variable or rigid, we can't reduce yet
            if !matches!(tail.as_ref(), Ty::Empty) {
                return None;
            }
            // Apply the value mapper to each field type
            let mapped_fields = fields
                .iter()
                .map(|(name, ty)| (name.clone(), mapper.apply(ty.clone())))
                .collect();
            Some(Ty::Row(mapped_fields, Box::new(Ty::Empty)))
        }
        Ty::Var(_) | Ty::Rigid(..) => None, // Open row: defer
        _ => None,
    }
}

pub(crate) fn directional_string_kind(marker: KeyMarker, m: Ty) -> Ty {
    let name = match marker {
        KeyMarker::Prefix => "prefixAffix",
        KeyMarker::Suffix => "suffixAffix",
        _ => unreachable!("only prefix and suffix stages carry a mapper"),
    };
    Ty::Con(name, vec![m])
}

/// The string a type-position literal denotes, if it is one.
///
/// Used for the affix of `(prefix "u_")` / `(suffix "_v2")`. A mapper's string
/// has to be a literal because it decides the output row's labels, so a
/// variable here is an error the caller reports.
pub(crate) fn string_literal_of(t: &TypeExpr) -> Option<String> {
    match t {
        TypeExpr::Str(s, _) => Some(s.clone()),
        _ => None,
    }
}

/// Compute the kind of a type. Variables and rigid variables look up their
/// kind in the `vars` vector (passed separately to avoid borrowing issues).
pub(crate) fn kind_of(t: &Ty, vars: &[VarInfo]) -> Kind {
    match t {
        Ty::Var(v) => vars[*v as usize].kind,
        Ty::Rigid(v, _) => vars[*v as usize].kind,
        Ty::Gen(_) => Kind::Type, // schemes handle their own kinding
        // `prefixAffix` / `suffixAffix` are string-valued affix parameters;
        // their KeyMap argument is metadata tying the argument to its row map.
        Ty::Con("prefixAffix" | "suffixAffix", _) => Kind::Type,
        Ty::Con("KeyMapId" | "KeyMapCompose", _) => Kind::KeyMap,
        Ty::Con(_, _) => Kind::Type,
        Ty::Fun(_, _) => Kind::Type,
        Ty::Row(_, _) => Kind::Row,
        Ty::Empty => Kind::Row,
        Ty::MapKey(_, _) => Kind::Row,
        Ty::Merge(_, _) => Kind::Row,
        Ty::MapValue(_, _) => Kind::Row,
        // A key affix is a `KeyMap`-kinded symbol: the argument of `keyMap`.
        Ty::KeyAffix(_, _) => Kind::KeyMap,
    }
}

pub(crate) const SCALARS: &[&str] = &["int", "float", "string", "bool", "date", "timestamp"];

/// Type constructors usable in signatures, with their arities.
pub(crate) const CONS: &[(&str, usize)] = &[
    ("int", 0),
    ("float", 0),
    ("string", 0),
    ("bool", 0),
    ("date", 0),
    ("timestamp", 0),
    ("maybe", 1),
    ("list", 1),
    ("query", 1),
    ("expr", 2),
    ("join", 2),
    ("agg", 1),
    ("win", 1),
    ("winspec", 1),
    ("sortkey", 1),
    ("frame", 0),
    ("bound", 0),
    // `row s` is the type of a record-of-columns expression, i.e. the argument
    // of `select`/`update`/`agg`. It is a distinct constructor from `s` itself
    // because a bare row used as a *value* would be a different (and
    // nonsensical) thing: `row` marks a row-phase record in expression
    // position. Its argument is a Row.
    // `keyMap m r` — the row term for a key mapping. `keymapper m` binds a
    // mapper variable of kind KeyMap.
    ("keyMap", 2),
    ("keymapper", 1),
    // The affix marker carries the concrete KeyMap witness selected by the
    // application. Keeping that witness in the argument is what ties
    // `prefix`/`suffix` to the mapper in their result row.
    ("prefixAffix", 1),
    ("suffixAffix", 1),
    // The key-parameter markers. A key stage reads its key from the
    // application, where the literal still is, so its parameter carries a
    // marker (and prefix/suffix also carry their KeyMap witness) instead of a
    // value type; `key_stage` recognises it and records the literal in the
    // stage's constraint or witness.
    ("key_omit", 0),
    ("key_prefix", 0),
    ("key_suffix", 0),
    ("value_wrapper", 0),
    // `merge r s` — the row former for a join's output row.
    ("merge", 2),
    // `mapValue w r` — the row term for a uniform type wrapper. `w` is the
    // wrapper marker, read at the application like a key.
    ("mapValue", 2),
    ("valuemapper", 1),
];

/// Returns the expected kinds for each type constructor's arguments.
/// Most take Type arguments, but query/winspec/sortkey take Row arguments.
pub(crate) fn expected_arg_kinds(con: &str) -> Vec<Kind> {
    match con {
        "query" => vec![Kind::Row],
        "join" => vec![Kind::Row, Kind::Row],
        "winspec" => vec![Kind::Row],
        "sortkey" => vec![Kind::Row],
        "expr" => vec![Kind::Row, Kind::Type],
        "prefixAffix" | "suffixAffix" => vec![Kind::KeyMap],
        "maybe" | "list" => vec![Kind::Type],
        "agg" | "win" => vec![Kind::Type], // These are handled specially in conv
        _ => vec![],
    }
}

pub(crate) const NULLABLE: &str =
    "expected a non-null value, found a `maybe`; use `coalesce default x` \
                        (or `isNull` / `isNotNull` to test it)";

/// An aggregate reaching a phase that must be row or window: next to a
/// plain column, or as a `select` field.
pub(crate) const UNGROUPED: &str =
    "aggregates cannot mix with ungrouped columns or be `select` fields; \
                         aggregate in `agg`, with columns wrapped in `group`";

/// How many forward-referenced definitions one scheme may resolve before the
/// checker reports the chain instead of overflowing the native stack. A
/// `def_scheme` level costs tens of kilobytes of stack, and a checker runs on
/// threads as small as a 2 MB test or embedder thread, so this stays well
/// under that. Real code defines a name before the definitions that use it.
pub(crate) const MAX_DEF_DEPTH: usize = 24;

/// The IR phase a phase type stands for, once it is known.
pub(crate) fn phase_of(t: &Ty) -> Option<Phase> {
    match t {
        Ty::Con("row", _) => Some(Phase::Row),
        Ty::Con("agg", _) => Some(Phase::Agg),
        Ty::Con("win", _) => Some(Phase::Win),
        _ => None,
    }
}

pub(crate) fn row_or_tail(fs: Vec<(String, Ty)>, tail: Ty) -> Ty {
    if fs.is_empty() {
        tail
    } else {
        row(fs, tail)
    }
}

pub(crate) fn contains_nullable_expr(t: &TypeExpr) -> bool {
    match t {
        TypeExpr::App { head, args, .. } => {
            (head == "expr"
                && args.len() == 2
                && matches!(&args[1], TypeExpr::App { head, .. } if head == "maybe"))
                || args.iter().any(contains_nullable_expr)
        }
        TypeExpr::Record { fields, .. } => fields.iter().any(|(_, t)| contains_nullable_expr(t)),
        TypeExpr::Fun(a, b) => contains_nullable_expr(a) || contains_nullable_expr(b),
        // A type-position string is an affix, not an expression type.
        TypeExpr::Str(..) => false,
        TypeExpr::Error(_) => false,
    }
}

pub(crate) fn lit_name(l: &ast::Lit) -> &'static str {
    match l {
        ast::Lit::Int(_) => "int",
        ast::Lit::Float(_) => "float",
        ast::Lit::Str(_) => "string",
        ast::Lit::Bool(_) => "bool",
    }
}

/// `_+_` → `+` in messages.
pub(crate) fn op_name(n: &str) -> &str {
    match n.strip_prefix('_').and_then(|n| n.strip_suffix('_')) {
        Some(op) if !op.is_empty() => op,
        _ => n,
    }
}

pub(crate) fn is_join(t: &Ty) -> bool {
    matches!(t, Ty::Con("join", _))
}
