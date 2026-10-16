//! The row algebra: one representation for a row, and one reducer.
//!
//! A row is what every query stage agrees or disagrees about: `where`, `order`,
//! `limit`, and `distinct` keep it, `select` replaces it, `update` merges into
//! it, `omit` removes one label, `prefix`/`suffix` rename every label, and the
//! joins combine two of them. The checker used to answer "what are this row's
//! columns?" in several places at once — `flatten` walked the terms, each stage
//! step decided for itself what an unreduced term meant, and `zonk` normalized
//! them again — so a row that could not yet be inspected could be treated as
//! equal to one that could.
//!
//! This module is the single owner of that question. [`Row`] is the row
//! language of `Ty`'s row constructors written as its own type; [`reduce_row`]
//! is the reducer. It answers with a *structured* result — a closed row, an
//! open row (its fields so far and the tail it could not reduce), or a specific
//! failure — and never with a plausible partial row.
//!
//! The reducer needs three things from the checker (following bound variables,
//! decoding a label-driven mapper, and resolving inside a constructor), so they
//! are asked for through [`RowEnv`] rather than by reaching into a `Checker`.
//! That is what makes the algebra testable on its own: a test supplies an
//! environment with no bindings and reduces the same rows the checker reduces.
//!
//! The first-order reducers themselves (`reduce_mapkey`, `reduce_merge`,
//! `reduce_mapvalue`, `reduce_removekey`) stay in [`super::reduce`], next to
//! the kind tables they belong to; this module owns *when* they run and what
//! their answer means.

use super::*;

/// A row, as the checker's row operations see it.
///
/// Total over `Ty`: anything that is not a row term becomes [`Row::Tail`], so
/// the reducer can carry an opaque tail instead of guessing at a shape. The
/// conversion is lossless in both directions ([`Row::of_ty`], [`Row::to_ty`]),
/// which is what lets the reducer work on this type while every caller keeps
/// passing and receiving `Ty`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Row {
    /// A row variable that is not bound yet.
    Var(u32),
    /// A rigid tail from a signature.
    Rigid(u32, String),
    /// A tail the reducer cannot look inside: a constructor, a function, or any
    /// other type that is not a row term.
    Tail(Ty),
    /// No fields and no tail.
    Empty,
    /// Fields, then the rest of the row.
    Fields(Vec<(String, Ty)>, Box<Row>),
    /// Every field label transformed by the mapper (a `Ty`, since it may still
    /// be a variable).
    MapKeys(Ty, Box<Row>),
    /// One label (a `Ty`: a `Sym`, or a variable waiting for one) removed.
    Remove(Ty, Box<Row>),
    /// Two rows combined; right wins on a collision.
    Merge(Box<Row>, Box<Row>),
    /// Every field type wrapped by the witness a label names.
    MapValues(Ty, Box<Row>),
}

impl Row {
    /// Read a `Ty` as a row. Total: a type that is not a row term becomes
    /// [`Row::Tail`].
    pub(crate) fn of_ty(t: &Ty) -> Row {
        match t {
            Ty::Var(v) => Row::Var(*v),
            Ty::Rigid(v, n) => Row::Rigid(*v, n.clone()),
            Ty::Empty => Row::Empty,
            Ty::Row(fs, tail) => Row::Fields(fs.clone(), Box::new(Row::of_ty(tail))),
            Ty::MapKey(km, row) => Row::MapKeys((**km).clone(), Box::new(Row::of_ty(row))),
            Ty::RemoveKey(k, row) => Row::Remove((**k).clone(), Box::new(Row::of_ty(row))),
            Ty::Merge(left, right) => {
                Row::Merge(Box::new(Row::of_ty(left)), Box::new(Row::of_ty(right)))
            }
            Ty::MapValue(label, row) => Row::MapValues((**label).clone(), Box::new(Row::of_ty(row))),
            other => Row::Tail(other.clone()),
        }
    }

    /// The `Ty` this row denotes. The inverse of [`Row::of_ty`].
    pub(crate) fn to_ty(&self) -> Ty {
        match self {
            Row::Var(v) => Ty::Var(*v),
            Row::Rigid(v, n) => Ty::Rigid(*v, n.clone()),
            Row::Tail(t) => t.clone(),
            Row::Empty => Ty::Empty,
            Row::Fields(fs, tail) => row(fs.clone(), tail.to_ty()),
            Row::MapKeys(km, row) => Ty::MapKey(Box::new(km.clone()), Box::new(row.to_ty())),
            Row::Remove(k, row) => Ty::RemoveKey(Box::new(k.clone()), Box::new(row.to_ty())),
            Row::Merge(left, right) => Ty::Merge(Box::new(left.to_ty()), Box::new(right.to_ty())),
            Row::MapValues(label, row) => Ty::MapValue(Box::new(label.clone()), Box::new(row.to_ty())),
        }
    }
}

/// What the reducer needs from the checker.
///
/// A trait rather than a `&Checker` so the reducer is a function of the
/// bindings it is handed, and so the algebra can be tested without a module
/// environment. The three methods are exactly the checker's own
/// [`Checker::resolve`], [`Checker::resolve_mapper`], and [`Checker::zonk`].
pub(crate) trait RowEnv {
    /// Follow a chain of bound variables to the first thing that is not one.
    fn resolve(&self, t: &Ty) -> Ty;
    /// A mapper resolved to its concrete form: a label-driven witness
    /// (`labelPrefix (Sym s)`) becomes the `KeyAffix` it stands for once its
    /// label is bound.
    fn resolve_mapper(&self, t: &Ty) -> Ty;
    /// Resolve inside constructors, for a label that lives in one.
    fn zonk(&self, t: &Ty) -> Ty;
}

impl RowEnv for Checker<'_> {
    fn resolve(&self, t: &Ty) -> Ty {
        Checker::resolve(self, t)
    }
    fn resolve_mapper(&self, t: &Ty) -> Ty {
        Checker::resolve_mapper(self, t)
    }
    fn zonk(&self, t: &Ty) -> Ty {
        Checker::zonk(self, t)
    }
}

/// What the reducer found.
///
/// The three answers are deliberately distinct: a caller that cannot report a
/// failure (a comparison through a term, say) still has to be able to carry the
/// term forward, and a caller that owns the operation has to be able to tell
/// "not yet" from "not there".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowOutcome {
    /// Every field is known and the tail is closed.
    Closed(Vec<(String, Ty)>),
    /// The fields known so far, and the tail the reducer could not reduce: a
    /// variable, a rigid tail, or a term still waiting on an operand.
    Open(Vec<(String, Ty)>, Ty),
    /// The operation cannot succeed, and the stage that owns it reports this.
    /// `fields` and `tail` describe the term that failed, so a caller that has
    /// no way to report it can carry the term forward unchanged.
    Failed {
        fields: Vec<(String, Ty)>,
        tail: Ty,
        error: RowError,
    },
}

impl RowOutcome {
    /// The fields known so far, and the tail they hang from. A failure reads
    /// exactly like an open row here: the difference is the [`RowError`], which
    /// only the operation's owner reports.
    pub(crate) fn parts(self) -> (Vec<(String, Ty)>, Ty) {
        match self {
            RowOutcome::Closed(fields) => (fields, Ty::Empty),
            RowOutcome::Open(fields, tail) => (fields, tail),
            RowOutcome::Failed { fields, tail, .. } => (fields, tail),
        }
    }
}

/// A row operation that cannot succeed, with the operation's own wording.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowError {
    /// `omit k` where the row has no column `k`. The label is known and the row
    /// is closed, so the column really is absent — the case a comparison
    /// through the term must not report, because the row may still grow.
    MissingLabel {
        label: String,
        fields: Vec<(String, Ty)>,
    },
    /// `mapValue w` where `w` names no wrapper the compiler knows.
    UnknownWrapper { name: String },
}

impl RowError {
    /// The diagnostic for the stage that owns the obligation.
    pub(crate) fn message(&self) -> String {
        match self {
            RowError::MissingLabel { label, fields } => missing(label, fields),
            RowError::UnknownWrapper { name } => format!("unknown wrapper `{name}`"),
        }
    }
}

/// Reduce a row to its columns, its remaining tail, or a failure.
///
/// This is the single reducer: `flatten` reads rows through it, and a stage
/// that owns a row rule asks it directly. A `keyMap` term composes with the
/// term beneath it, a `removeKey` removes one label, a `merge` combines two
/// rows right-wins, and a `mapValue` wraps every field type; each is reduced as
/// soon as its operands are known, and retained otherwise.
pub(crate) fn reduce_row(env: &impl RowEnv, t: &Ty) -> RowOutcome {
    reduce(env, t.clone())
}

/// The reducer proper. Resolves its argument first, so every recursive call is
/// made against the row as it is *now*: a term built before its tail was bound
/// is the common case, and reading it unresolved is what makes a reducible row
/// look open.
fn reduce(env: &impl RowEnv, t: Ty) -> RowOutcome {
    let mut fields: Vec<(String, Ty)> = Vec::new();
    let mut cur = env.resolve(&t);
    loop {
        match Row::of_ty(&cur) {
            // The row is closed, or its tail is something the reducer cannot
            // look inside. Neither is an error: `Open` says what is known.
            Row::Empty => return RowOutcome::Closed(fields),
            tail @ (Row::Var(_) | Row::Rigid(..) | Row::Tail(_)) => {
                return RowOutcome::Open(fields, tail.to_ty())
            }
            Row::Fields(more, tail) => {
                fields.extend(more);
                cur = env.resolve(&tail.to_ty());
            }
            Row::MapKeys(km, inner) => {
                // Resolve both components before trying to reduce: a pipeline
                // often binds the mapper and the inner row through separate
                // applications, so leaving either child a variable makes an
                // otherwise closed term look open.
                let km = env.resolve_mapper(&km);
                let (inner_fields, inner_tail) = reduce(env, inner.to_ty()).parts();
                let row = row_or_tail(inner_fields, inner_tail);
                match reduce_mapkey(&km, &row) {
                    Some(reduced) => cur = env.resolve(&reduced),
                    None => {
                        return RowOutcome::Open(
                            fields,
                            Ty::MapKey(Box::new(km), Box::new(row)),
                        )
                    }
                }
            }
            Row::Remove(label, inner) => {
                // The label is usually a variable bound at the call site, and
                // it lives inside a constructor, so it is zonked rather than
                // merely resolved.
                let label = env.zonk(&label);
                let (inner_fields, inner_tail) = reduce(env, inner.to_ty()).parts();
                let row = row_or_tail(inner_fields.clone(), inner_tail.clone());
                match reduce_removekey(&label, &row) {
                    Some(reduced) => cur = env.resolve(&reduced),
                    None => {
                        let tail = Ty::RemoveKey(Box::new(label.clone()), Box::new(row));
                        // A known label over a closed row is the one case that
                        // is a failure rather than a wait: there is nothing
                        // left to bind, so the column really is absent.
                        if let (Ty::Sym(name), Ty::Empty) = (&label, &inner_tail) {
                            return RowOutcome::Failed {
                                fields,
                                tail,
                                error: RowError::MissingLabel {
                                    label: name.clone(),
                                    fields: inner_fields,
                                },
                            };
                        }
                        return RowOutcome::Open(fields, tail);
                    }
                }
            }
            Row::Merge(left, right) => {
                // Both operands are flattened before the merge is attempted: a
                // join binds each side through its own constraint, so the merge
                // often becomes closed only after this term was built, and a row
                // tail can itself be a row after unification.
                let (lf, lt) = reduce(env, env.resolve(&left.to_ty())).parts();
                let (rf, rt) = reduce(env, env.resolve(&right.to_ty())).parts();
                let left = row_or_tail(lf, lt);
                let right = row_or_tail(rf, rt);
                match reduce_merge(&left, &right) {
                    Some(reduced) => cur = env.resolve(&reduced),
                    None => {
                        return RowOutcome::Open(
                            fields,
                            Ty::Merge(Box::new(left), Box::new(right)),
                        )
                    }
                }
            }
            Row::MapValues(label, inner) => {
                // The label lives inside a constructor until the use site binds
                // it, so it is zonked rather than merely resolved.
                let label = env.zonk(&label);
                let (inner_fields, inner_tail) = reduce(env, inner.to_ty()).parts();
                let row = row_or_tail(inner_fields, inner_tail.clone());
                match reduce_mapvalues(&label, &row) {
                    Some(reduced) => cur = env.resolve(&reduced),
                    None => {
                        let tail = Ty::MapValue(Box::new(label.clone()), Box::new(row));
                        // A known label that names no wrapper, over a closed
                        // row, is a failure rather than a wait: there is
                        // nothing left to bind.
                        if let Ty::Sym(name) = &label {
                            if matches!(inner_tail, Ty::Empty) && value_map_of(name).is_none() {
                                return RowOutcome::Failed {
                                    fields,
                                    tail,
                                    error: RowError::UnknownWrapper { name: name.clone() },
                                };
                            }
                        }
                        return RowOutcome::Open(fields, tail);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reducer environment with no bindings: the rows as they are written,
    /// before anything has been solved.
    struct Unbound;

    impl RowEnv for Unbound {
        fn resolve(&self, t: &Ty) -> Ty {
            t.clone()
        }
        fn resolve_mapper(&self, t: &Ty) -> Ty {
            t.clone()
        }
        fn zonk(&self, t: &Ty) -> Ty {
            t.clone()
        }
    }

    /// The names the reducer found, or a panic naming the failure.
    fn names(outcome: RowOutcome) -> Vec<String> {
        match outcome {
            RowOutcome::Closed(fields) => fields.into_iter().map(|(n, _)| n).collect(),
            other => panic!("expected a closed row, got {other:?}"),
        }
    }

    fn two_columns() -> Ty {
        row(
            vec![
                ("id".to_string(), con("int")),
                ("name".to_string(), con("string")),
            ],
            Ty::Empty,
        )
    }

    fn prefixed(affix: &str, row: Ty) -> Ty {
        Ty::MapKey(
            Box::new(KeyMap::Prefix(affix.to_string()).to_ty()),
            Box::new(row),
        )
    }

    #[test]
    fn adjacent_mappers_compose_like_one_mapper() {
        // `MapKeys(g, MapKeys(f, r))` is the same row as the composed mapper,
        // whichever order the two terms were built in. This is the property the
        // stage boundary depends on: two renames in a body are two terms, and
        // they must reduce to one row.
        let nested = Ty::MapKey(
            Box::new(KeyMap::Suffix("_v2".to_string()).to_ty()),
            Box::new(prefixed("u_", two_columns())),
        );
        let composed = Ty::MapKey(
            Box::new(
                KeyMap::Compose(
                    Box::new(KeyMap::Prefix("u_".to_string())),
                    Box::new(KeyMap::Suffix("_v2".to_string())),
                )
                .normalize()
                .to_ty(),
            ),
            Box::new(two_columns()),
        );
        assert_eq!(names(reduce_row(&Unbound, &nested)), vec!["u_id_v2", "u_name_v2"]);
        assert_eq!(
            names(reduce_row(&Unbound, &nested)),
            names(reduce_row(&Unbound, &composed))
        );
    }

    #[test]
    fn a_closed_rename_agrees_with_the_schema_rule() {
        // The checked IR renames columns with `schema::rename_columns`; the
        // checker must reach the same names, or a query's type and its SQL
        // would disagree about what the columns are called.
        for (affix, prefix) in [("u_", true), ("_v2", false)] {
            let km = if prefix {
                KeyMap::Prefix(affix.to_string())
            } else {
                KeyMap::Suffix(affix.to_string())
            };
            let reduced = names(reduce_row(&Unbound, &Ty::MapKey(Box::new(km.to_ty()), Box::new(two_columns()))));
            let columns: Vec<String> = vec!["id".into(), "name".into()];
            assert_eq!(reduced, crate::schema::rename_columns(&columns, affix, prefix));
        }
    }

    #[test]
    fn an_open_tail_reports_the_columns_it_has_read() {
        // A rename is a term, and a column's *position* relative to it decides
        // whether the rename reaches it. Both shapes below are open, and each
        // reports exactly the columns it has read so far — which is the fact a
        // stage boundary rests on: a row that cannot be inspected is neither a
        // row with no columns nor a row that is complete.
        let id = ("id".to_string(), con("int"));
        let age = ("age".to_string(), con("int"));
        // The mapper is outside, so `id` is inside the term that cannot be
        // reduced yet: nothing has been read.
        let outside = Ty::MapKey(
            Box::new(KeyMap::Prefix("u_".to_string()).to_ty()),
            Box::new(row(vec![id.clone()], Ty::Var(0))),
        );
        match reduce_row(&Unbound, &outside) {
            RowOutcome::Open(fields, tail) => {
                assert!(fields.is_empty(), "the column is inside the term: {fields:?}");
                assert!(matches!(tail, Ty::MapKey(..)), "{tail:?}");
            }
            other => panic!("expected an open row, got {other:?}"),
        }
        // The mapper is inside, so `id` precedes it: already read, and *outside*
        // the rename — it keeps its name.
        let inside = row(vec![id.clone()], prefixed("u_", Ty::Var(0)));
        match reduce_row(&Unbound, &inside) {
            RowOutcome::Open(fields, tail) => {
                assert_eq!(fields.len(), 1, "the leading column is known");
                assert!(matches!(tail, Ty::MapKey(..)), "{tail:?}");
            }
            other => panic!("expected an open row, got {other:?}"),
        }
        // Binding the tail closes both, and the mapper renames exactly the
        // columns it encloses: both of them in the first shape, only the tail's
        // in the second.
        let tail = row(vec![age.clone()], Ty::Empty);
        let bound_outside = Ty::MapKey(
            Box::new(KeyMap::Prefix("u_".to_string()).to_ty()),
            Box::new(row(vec![id.clone()], tail.clone())),
        );
        let bound_inside = row(vec![id.clone()], prefixed("u_", tail));
        assert_eq!(
            names(reduce_row(&Unbound, &bound_outside)),
            vec!["u_id", "u_age"]
        );
        assert_eq!(
            names(reduce_row(&Unbound, &bound_inside)),
            vec!["id", "u_age"]
        );
    }

    #[test]
    fn a_missing_label_is_the_operations_error() {
        // A known label over a closed row is a failure, not a wait.
        let removed = Ty::RemoveKey(
            Box::new(Ty::Sym("nope".to_string())),
            Box::new(two_columns()),
        );
        match reduce_row(&Unbound, &removed) {
            RowOutcome::Failed { error, .. } => {
                assert!(error.message().contains("no column `nope`"), "{error:?}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        // The same removal over an open row waits instead: the column may still
        // arrive.
        let waiting = Ty::RemoveKey(
            Box::new(Ty::Sym("nope".to_string())),
            Box::new(Ty::Var(0)),
        );
        assert!(matches!(
            reduce_row(&Unbound, &waiting),
            RowOutcome::Open(..)
        ));
        // And an unbound label waits too, whatever the row says.
        let unbound = Ty::RemoveKey(Box::new(Ty::Var(1)), Box::new(two_columns()));
        assert!(matches!(
            reduce_row(&Unbound, &unbound),
            RowOutcome::Open(..)
        ));
    }

    #[test]
    fn a_removal_keeps_the_order_of_the_rest() {
        let removed = Ty::RemoveKey(
            Box::new(Ty::Sym("id".to_string())),
            Box::new(two_columns()),
        );
        assert_eq!(names(reduce_row(&Unbound, &removed)), vec!["name"]);
    }

    #[test]
    fn an_unknown_tail_is_carried_not_guessed() {
        // A row whose tail is a rigid name from a signature has the fields it
        // lists and no more: the reducer says so instead of closing the row.
        let open = row(
            vec![("id".to_string(), con("int"))],
            Ty::Rigid(3, "t".to_string()),
        );
        match reduce_row(&Unbound, &open) {
            RowOutcome::Open(fields, tail) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(tail, Ty::Rigid(3, "t".to_string()));
            }
            other => panic!("expected an open row, got {other:?}"),
        }
        // A type that is not a row at all is a tail too, not an empty row.
        assert_eq!(
            reduce_row(&Unbound, &con("int")).parts(),
            (vec![], con("int"))
        );
    }

    #[test]
    fn a_merge_reduces_right_wins() {
        let left = row(
            vec![
                ("id".to_string(), con("int")),
                ("name".to_string(), con("string")),
            ],
            Ty::Empty,
        );
        let right = row(
            vec![
                ("name".to_string(), con("float")),
                ("extra".to_string(), con("bool")),
            ],
            Ty::Empty,
        );
        let merged = reduce_row(&Unbound, &Ty::Merge(Box::new(left), Box::new(right)));
        let RowOutcome::Closed(fields) = merged else {
            panic!("expected a closed merge, got {merged:?}");
        };
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "extra"]);
        // Right wins on the collision, and the position is the left's.
        assert_eq!(fields[1].1, con("float"));
    }

    #[test]
    fn a_value_map_wraps_every_field_type() {
        let mapped = reduce_row(
            &Unbound,
            &Ty::MapValue(
                Box::new(Ty::Sym("nullable".to_string())),
                Box::new(two_columns()),
            ),
        );
        let RowOutcome::Closed(fields) = mapped else {
            panic!("expected a closed row, got {mapped:?}");
        };
        assert_eq!(fields[0].1, Ty::Con("maybe", vec![con("int")]));
        assert_eq!(fields[1].1, Ty::Con("maybe", vec![con("string")]));
    }
}
