//! Phase and join-side rules shared by the type checker (on phase types)
//! and the IR validator (on IR phases), so both accept the same programs
//! and report them the same way.

use crate::ir::Phase;

/// A join-side column (`.<x`, `.>x`) outside a join predicate.
pub const JOIN_ONLY: &str =
    "`.<x` and `.>x` refer to the inputs of a join and can only be used in a join predicate";

/// Fields of a window spec record (`{ partition = [..], order = [..], frame = .. }`).
/// The checker gives them their types and the evaluator reads them, so the
/// names live here instead of being spelled out in both.
pub mod winspec {
    pub const PARTITION: &str = "partition";
    pub const ORDER: &str = "order";
    pub const FRAME: &str = "frame";

    /// Every field, in the order diagnostics list them.
    pub const ALL: &[&str] = &[PARTITION, ORDER, FRAME];

    /// `partition, order, frame`, for an error message.
    pub fn names() -> String {
        ALL.join(", ")
    }

    /// Is `name` a window spec field?
    pub fn is_field(name: &str) -> bool {
        ALL.contains(&name)
    }
}

/// Is `name` — the `_op_` spelling an operator desugars to — one of the
/// pipeline stage shorthands? A stage built with one of these is located at
/// its argument (the stage) rather than at the whole pipeline, so a
/// diagnostic points at the stage that failed.
///
/// Which operators are stages is not listed here: an operator is a stage when
/// it is declared at `&`'s level in `prelude.cagara`, so the language, not
/// Rust, decides what a shorthand means.
pub fn is_pipe_name(name: &str) -> bool {
    cagara_syntax::ops().is_stage_name(name)
}

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
        (Place::Agg, Win) => "is a window function; compute it in a `select` stage after `agg`",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The names the checker and the evaluator must agree on. Both used to
    /// spell them out, so a rename in one place silently desynchronized them.
    #[test]
    fn shared_names_are_the_ones_the_prelude_uses() {
        assert_eq!(winspec::ALL, ["partition", "order", "frame"]);
        assert!(winspec::is_field("partition"));
        assert!(!winspec::is_field("qualify"));
        assert_eq!(winspec::names(), "partition, order, frame");
    }

    /// Every stage operator is declared *and* defined in the prelude: the
    /// declaration gives it its place in the grammar, the definition gives it
    /// its meaning, and neither works alone.
    #[test]
    fn every_stage_operator_is_declared_and_defined_in_the_prelude() {
        let prelude = include_str!("../../../prelude.cagara");
        let ops = cagara_syntax::ops();
        let stages = ops.stages();
        assert!(
            !stages.is_empty(),
            "the prelude declares no stage operators"
        );
        for spelling in stages {
            let pipe = ops.pipe_fixity().expect("the prelude declares `&`");
            assert_eq!(
                ops.find(spelling).map(|f| f.precedence()),
                Some(pipe.precedence()),
                "`{spelling}` is a stage, so it must be declared at `&`'s level"
            );
            let name = cagara_syntax::op_name(spelling);
            assert!(
                prelude.contains(&format!("{name} :")),
                "`{spelling}` is a stage operator but `{name}` is not defined in the prelude"
            );
        }
        // The pipeline itself and every shorthand, so a new one cannot be
        // declared without the rest of the language noticing.
        for name in ["_&_", "_&=_", "_&?_", "_&+_", "_&*_", "_&._", "_&-_"] {
            assert!(is_pipe_name(name), "`{name}` must be a stage operator");
        }
        // Join operators are declared too, but they are not stages.
        for name in ["_?_", "_<?_", "_?>_", "_<?>_"] {
            assert!(!is_pipe_name(name), "`{name}` is a join, not a stage");
        }
        assert!(!is_pipe_name("_+_"));
        assert!(!is_pipe_name("plain"));
    }
}
