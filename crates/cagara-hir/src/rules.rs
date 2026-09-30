//! Phase and join-side rules shared by the type checker (on phase types)
//! and the IR validator (on IR phases), so both accept the same programs
//! and report them the same way.

use crate::ir::Phase;

/// A join-side column (`.<x`, `.>x`) outside a join predicate.
pub const JOIN_ONLY: &str =
    "`.<x` and `.>x` refer to the inputs of a join and can only be used in a join predicate";

/// A plain column (`.x`) in a join predicate.
pub fn needs_side(n: &str) -> String {
    format!("join predicates must say which input a column comes from: `.<{n}` (left) or `.>{n}` (right)")
}

/// Where an expression is used by a query stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Where,
    /// A field of `select`.
    Select,
    /// A field of `agg`.
    Agg,
    /// A sort key, or a window's partition / order key.
    Key,
    JoinOn,
}

/// May an expression of phase `p` be used at `place`? Messages for
/// `select` / `agg` fields follow the field's label (`field `x` ...`).
pub fn place(place: Place, p: Phase) -> Result<(), String> {
    use Phase::*;
    let msg = match (place, p) {
        (_, Const) => return Ok(()),
        (Place::Where, Row) | (Place::Key, Row) | (Place::JoinOn, Row) => return Ok(()),
        (Place::Select, Row | Win) | (Place::Agg, Agg) => return Ok(()),
        (Place::Where, Agg) => {
            "`where` cannot filter on an aggregate; filter the output of an `agg` stage instead"
        }
        (Place::Where, Win) => {
            "`where` cannot filter on a window function; `select` it first, then filter the new column"
        }
        (Place::Select, Agg) => "is an aggregate; aggregates belong in `agg`, not `select`",
        (Place::Agg, Row) => {
            "uses a column that is not grouped; wrap it in `group` or aggregate it"
        }
        (Place::Agg, Win) => "is a window function; use it in a `select` stage after `agg`",
        (Place::Key, Agg | Win) => {
            "sort and partition keys must be plain column expressions; \
             compute aggregates or windows in an earlier stage"
        }
        (Place::JoinOn, Agg | Win) => {
            "join predicates cannot contain aggregates or window functions"
        }
    };
    Err(msg.into())
}

/// Phase of an expression built from parts of phases `a` and `b`.
pub fn mix(a: Phase, b: Phase) -> Result<Phase, String> {
    use Phase::*;
    match (a, b) {
        (Const, p) | (p, Const) => Ok(p),
        (Row, Row) => Ok(Row),
        (Agg, Agg) => Ok(Agg),
        (Win, Win) | (Row, Win) | (Win, Row) => Ok(Win),
        _ => Err(clash(a, b)),
    }
}

/// Why parts of phases `a` and `b` cannot be combined.
pub fn clash(a: Phase, b: Phase) -> String {
    use Phase::*;
    match (a, b) {
        (Agg, Win) | (Win, Agg) => {
            "mixes an aggregate with a window function; use `agg` first, then `select` the window"
                .into()
        }
        (Agg, _) | (_, Agg) => "aggregates cannot nest or mix with ungrouped columns; wrap \
                                columns in `group`, or aggregate in an earlier `agg` stage"
            .into(),
        _ => "window functions cannot nest; compute the inner window in an earlier `select` stage"
            .into(),
    }
}

/// An argument of an aggregate, group key, or window function (`what`)
/// must be a row expression: aggregates and windows do not nest.
pub fn nested(what: &str, p: Phase) -> Result<(), String> {
    match p {
        Phase::Const | Phase::Row => Ok(()),
        Phase::Agg => Err(format!(
            "{what} contains an aggregate; aggregates cannot nest (aggregate in an earlier `agg` stage)"
        )),
        Phase::Win => Err(format!(
            "{what} contains a window function; windows cannot nest (compute it in an earlier `select` stage)"
        )),
    }
}

/// Output columns of a join: the left input's, then the right input's that
/// the left does not have (the left one wins on a name collision).
pub fn join_columns<T: Clone>(left: &[(String, T)], right: &[(String, T)]) -> Vec<(String, T)> {
    let mut out = left.to_vec();
    out.extend(
        right
            .iter()
            .filter(|(n, _)| !left.iter().any(|(l, _)| l == n))
            .cloned(),
    );
    out
}
