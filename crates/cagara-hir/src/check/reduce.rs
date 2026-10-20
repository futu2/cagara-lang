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

    Some(Ty::Row(
        overlay(left_fields, right_fields),
        Box::new(Ty::Empty),
    ))
}

/// Overlay `right` on `left`: the left keeps its positions, a collision takes
/// the right's value, and a name only the right has is appended, in the right's
/// order.
///
/// One rule, in one place. The row reducer needs it at the type level and the
/// scheme reader needs it at the closed level, and two spellings of "right
/// wins" is how a signature and a query could come to disagree about which
/// column a join or an `update` produces.
pub(crate) fn overlay<T>(left: Vec<(String, T)>, right: Vec<(String, T)>) -> Vec<(String, T)> {
    let mut out = left;
    for (name, value) in right {
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => out.push((name, value)),
        }
    }
    out
}

/// The wrapper a `mapValue` label names, if it names one.
///
/// One table, read by the stage step that decodes the label and by the stage
/// node that checks its own row. A second copy would be a second answer waiting
/// to disagree.
pub(crate) fn value_map_of(name: &str) -> Option<ValueMap> {
    match name {
        "nullable" => Some(ValueMap::AsNullable),
        "list" => Some(ValueMap::AsList),
        "id" => Some(ValueMap::Id),
        _ => None,
    }
}

/// The wrapper a *label* names, if it names one.
///
/// A label is a `Sym` once the use site has bound it; an open label (a variable)
/// names nothing yet, which is the deferred case rather than an error.
pub(crate) fn value_map_of_label(label: &Ty) -> Option<ValueMap> {
    match label {
        Ty::Sym(name) => value_map_of(name),
        _ => None,
    }
}

/// Reduce a `mapValue` term: decode its label, then wrap every field type.
pub(crate) fn reduce_mapvalues(label: &Ty, row: &Ty) -> Option<Ty> {
    reduce_mapvalue(&value_map_of_label(label)?, row)
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
        // A literal type is an ordinary type: it unifies only with itself.
        Ty::Sym(_) => Kind::Type,
        Ty::Con("KeyMapId" | "KeyMapCompose" | "labelPrefix" | "labelSuffix", _) => Kind::KeyMap,
        Ty::Con(_, _) => Kind::Type,
        Ty::Fun(_, _) => Kind::Type,
        Ty::Row(_, _) => Kind::Row,
        Ty::Empty => Kind::Row,
        Ty::MapKey(_, _) => Kind::Row,
        Ty::Merge(_, _) => Kind::Row,
        Ty::MapValue(_, _) => Kind::Row,
        Ty::RemoveKey(_, _) => Kind::Row,
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
    // The key-parameter markers. A key stage reads its key from the
    // application, where the literal still is, so its parameter carries a
    // marker instead of a value type; `key_stage` recognises it and records the
    // literal in the stage's constraint or witness.
    // `valueLabel w` — the witness `mapValue` takes, carried as a label so the
    // operation travels in the type.
    ("valueLabel", 1),
    // `merge r s` — the row former for a join's output row.
    ("merge", 2),
    // `mapValue w r` — the row term for a uniform type wrapper. `w` is the
    // wrapper marker, read at the application like a key.
    ("mapValue", 2),
    ("valuemapper", 1),
    // `symbol k` — a column label, read at the application like a key. The
    // literal name is what `omit`/`prefix`/`suffix` take as their argument.
    ("symbol", 1),
    // `removeKey k r` — the row former for dropping one column. `k` is a
    // label type (`Sym` or a `symbol` variable); see `reduce_removekey`.
    ("removeKey", 2),
    // `prependSym k` / `appendSym k` — label-driven prefix and suffix mappers.
    ("prependSym", 1),
    ("appendSym", 1),
    ("labelPrefix", 1),
    ("labelSuffix", 1),
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

/// Reduce `removeKey k row` when `row` is closed. Removes the field named `k`
/// and keeps the order of the rest. Returns `None` while the row is open, or
/// when `k` is absent: the caller reports that as a missing column.
pub(crate) fn reduce_removekey(label: &Ty, row: &Ty) -> Option<Ty> {
    // The label is a type-level literal. A variable label is not known yet,
    // so the term waits.
    let Ty::Sym(k) = label else {
        return None;
    };
    match row {
        Ty::Empty => None,
        Ty::Row(fields, tail) if matches!(tail.as_ref(), Ty::Empty) => {
            if !fields.iter().any(|(n, _)| n == k) {
                return None;
            }
            let kept = fields.iter().filter(|(n, _)| n != k).cloned().collect();
            Some(Ty::Row(kept, Box::new(Ty::Empty)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod removekey_tests {
    use super::*;

    fn row3() -> Ty {
        row_or_tail(
            vec![
                ("a".into(), con("int")),
                ("b".into(), con("string")),
                ("c".into(), con("float")),
            ],
            Ty::Empty,
        )
    }

    #[test]
    fn removes_the_named_field_and_keeps_order() {
        let out = reduce_removekey(&Ty::Sym("b".into()), &row3()).expect("b is present");
        let (fs, tail) = match out {
            Ty::Row(fs, tail) => (fs, *tail),
            other => panic!("expected a row, got {other:?}"),
        };
        let names: Vec<&str> = fs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(tail, Ty::Empty);
    }

    #[test]
    fn absent_field_does_not_reduce() {
        assert!(reduce_removekey(&Ty::Sym("zz".into()), &row3()).is_none());
    }

    #[test]
    fn open_row_does_not_reduce() {
        let open = row_or_tail(vec![("a".into(), con("int"))], Ty::Rigid(0, "t".into()));
        assert!(reduce_removekey(&Ty::Sym("a".into()), &open).is_none());
    }

    #[test]
    fn variable_label_does_not_reduce() {
        // The label is still unknown, so the term must wait for it.
        let var_label = Ty::Rigid(0, "k".into());
        assert!(reduce_removekey(&var_label, &row3()).is_none());
    }
}

#[cfg(test)]
mod keymap_baseline_tests {
    use super::*;

    fn row2() -> Ty {
        row_or_tail(
            vec![("id".into(), con("int")), ("name".into(), con("string"))],
            Ty::Empty,
        )
    }

    fn names(t: &Ty) -> Vec<String> {
        match t {
            Ty::Row(fs, _) => fs.iter().map(|(n, _)| n.clone()).collect(),
            other => panic!("expected a row, got {other:?}"),
        }
    }

    #[test]
    fn prefix_renames_every_field_in_order() {
        let km = KeyMap::Prefix("u_".into()).to_ty();
        let out = reduce_mapkey(&km, &row2()).expect("closed row reduces");
        assert_eq!(names(&out), vec!["u_id", "u_name"]);
    }

    #[test]
    fn suffix_renames_every_field_in_order() {
        let km = KeyMap::Suffix("_v2".into()).to_ty();
        let out = reduce_mapkey(&km, &row2()).expect("closed row reduces");
        assert_eq!(names(&out), vec!["id_v2", "name_v2"]);
    }

    #[test]
    fn variable_mapper_does_not_reduce() {
        let km = Ty::Rigid(0, "m".into());
        assert!(reduce_mapkey(&km, &row2()).is_none());
    }
}

#[cfg(test)]
mod label_mapper_tests {
    use super::*;

    #[test]
    fn bound_label_decodes_to_prefix() {
        let t = Ty::Con("labelPrefix", vec![Ty::Sym("u_".into())]);
        assert_eq!(KeyMap::of_label_ty(&t), Some(KeyMap::Prefix("u_".into())));
    }

    #[test]
    fn bound_label_decodes_to_suffix() {
        let t = Ty::Con("labelSuffix", vec![Ty::Sym("_v2".into())]);
        assert_eq!(KeyMap::of_label_ty(&t), Some(KeyMap::Suffix("_v2".into())));
    }

    #[test]
    fn open_label_does_not_decode() {
        let t = Ty::Con("labelPrefix", vec![Ty::Rigid(0, "k".into())]);
        assert_eq!(KeyMap::of_label_ty(&t), None);
    }
}
