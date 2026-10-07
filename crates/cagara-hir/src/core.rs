//! The checked layer's data-only kernel: where a node came from, the scalar
//! types and closed rows the checker produces, and the row operations the
//! stage constructors share.
//!
//! Everything here is plain owned data. There is no `Rc`, no closure, and —
//! deliberately — no dependency on the type checker's internal `Ty`: the
//! checked layer is the *result* of checking, so it must not read the
//! mutable machinery that produced it. `Verified` is the one wrapper for
//! values that a constructor has already validated.

use crate::rules;
use crate::schema;
use crate::value::TplKind;
use crate::workspace::Diag;
use cagara_syntax::ast::Span;

/// Where a checked node was written.
///
/// The target design puts this on every node instead of wrapping the
/// relational tree in a transparent `Rel::At`, so a later phase can name the
/// node that made an operation invalid without unwrapping anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    pub module: usize,
    pub span: Span,
}

impl Origin {
    pub fn new(module: usize, span: Span) -> Self {
        Origin { module, span }
    }
}

/// A scalar type as the checked layer sees it: the shape the checker proved,
/// without variables, rigid names, or pending constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarType {
    Int,
    Float,
    String,
    Bool,
    Decimal,
    Date,
    Timestamp,
    Maybe(Box<ScalarType>),
    List(Box<ScalarType>),
    /// A type the checker reached the end of a query with still open, or one
    /// whose constructor it has no scalar spelling for. It unifies with
    /// nothing; a constructor that needs a definite type rejects it rather
    /// than guessing.
    Unknown,
}

impl ScalarType {
    /// Is this type not known? Only meaningful to the constructors that
    /// cannot decide without one (`where` needs a bool, joins need rows).
    pub fn is_unknown(&self) -> bool {
        matches!(self, ScalarType::Unknown)
    }

    /// Is this a `maybe` at the top level?
    pub fn is_maybe(&self) -> bool {
        matches!(self, ScalarType::Maybe(_))
    }

    /// Wrap in `maybe`. Idempotent, matching `ValueMap::AsNullable` in the
    /// checker and the `maybe (maybe t) = maybe t` law the language states.
    ///
    /// This is the shape `check::ty_to_scalar` calls when it reads a `maybe`
    /// out of a `Ty`, so the two agree on idempotence by construction.
    pub fn maybe(t: ScalarType) -> ScalarType {
        match t {
            t @ ScalarType::Maybe(_) => t,
            t => ScalarType::Maybe(Box::new(t)),
        }
    }

    /// Wrap in `maybe` (see [`ScalarType::maybe`]).
    pub fn nullable(self) -> ScalarType {
        ScalarType::maybe(self)
    }

    /// The scalar constructor this type is written with (`int`, `maybe`, …).
    ///
    /// A plain `Display` writes a *readable* type for a user; this is the
    /// spelling a `Ty` constructor uses, for comparing against one.
    pub fn pow_name(&self) -> &'static str {
        match self {
            ScalarType::Int => "int",
            ScalarType::Float => "float",
            ScalarType::Decimal => "decimal",
            ScalarType::String => "string",
            ScalarType::Bool => "bool",
            ScalarType::Date => "date",
            ScalarType::Timestamp => "timestamp",
            ScalarType::Maybe(_) => "maybe",
            ScalarType::List(_) => "list",
            ScalarType::Unknown => "?",
        }
    }

    /// The type inside a `maybe`, if there is one.
    pub fn strip_maybe(&self) -> Option<&ScalarType> {
        match self {
            ScalarType::Maybe(t) => Some(t),
            _ => None,
        }
    }

    /// The element type of a `list`, if this is one.
    pub fn elem(&self) -> Option<&ScalarType> {
        match self {
            ScalarType::List(t) => Some(t),
            _ => None,
        }
    }

    /// Is a value of this type usable as a condition? A `maybe bool` is not:
    /// the language requires `coalesce` first, the same way its operators
    /// refuse nullable arguments.
    pub fn is_bool(&self) -> bool {
        matches!(self, ScalarType::Bool)
    }
}

impl std::fmt::Display for ScalarType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarType::Int => f.write_str("int"),
            ScalarType::Float => f.write_str("float"),
            ScalarType::String => f.write_str("string"),
            ScalarType::Bool => f.write_str("bool"),
            ScalarType::Decimal => f.write_str("decimal"),
            ScalarType::Date => f.write_str("date"),
            ScalarType::Timestamp => f.write_str("timestamp"),
            ScalarType::Unknown => f.write_str("?"),
            ScalarType::Maybe(t) => write!(f, "maybe {t}"),
            ScalarType::List(t) => write!(f, "list {t}"),
        }
    }
}

/// A query's output row: ordered `(column, type)` pairs.
///
/// Order is part of the type — `select` exposes its fields in the order they
/// were written, and a set operation compares rows positionally — so this is
/// a `Vec`, not a map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowType {
    pub columns: Vec<(String, ScalarType)>,
}

impl RowType {
    pub fn new(columns: Vec<(String, ScalarType)>) -> Self {
        RowType { columns }
    }

    /// A row from names only, with every type still open. Completing a
    /// partially known row — the `PROBE_FIELD` case, where only the names
    /// are known — needs this.
    pub fn unknown(columns: Vec<String>) -> Self {
        RowType {
            columns: columns.into_iter().map(|n| (n, ScalarType::Unknown)).collect(),
        }
    }

    /// The columns in order.
    pub fn columns(&self) -> &[(String, ScalarType)] {
        &self.columns
    }

    /// The column names in order.
    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Does the row have a column of this name?
    pub fn has(&self, name: &str) -> bool {
        self.columns.iter().any(|(n, _)| n == name)
    }

    /// The type of `name`, if the row has it.
    pub fn get(&self, name: &str) -> Option<&ScalarType> {
        self.columns.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    /// Append a field (or replace it in place if the name is already there).
    pub fn with(mut self, name: impl Into<String>, ty: ScalarType) -> Self {
        let name = name.into();
        match self.columns.iter_mut().find(|(n, _)| *n == name) {
            Some((_, t)) => *t = ty,
            None => self.columns.push((name, ty)),
        }
        self
    }

    /// The first name that occurs more than once, if any.
    ///
    /// A row with a repeated name is not a valid output row: `select` and
    /// `agg` list their fields, and two fields with one name would produce a
    /// query whose row cannot address both. The checker rejects it through the
    /// record's field rule, and [`CheckedQuery::select`](crate::checked) rejects
    /// it again so a hand-built row cannot slip through.
    pub fn duplicate(&self) -> Option<&str> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.columns.len());
        for (n, _) in &self.columns {
            if seen.contains(&n.as_str()) {
                return Some(n);
            }
            seen.push(n);
        }
        None
    }

    /// `omit "k"`: the row without column `k`.
    ///
    /// This is the row *former*, so a key the row does not have is a no-op:
    /// the rule that rejects a missing key lives in the constructor that
    /// consumes this, [`CheckedQuery::omit`](crate::checked::CheckedQuery::omit),
    /// and in `schema::omit_columns` for an untyped tree. Making this total is
    /// what lets those two be the single place the rule is stated; do not read
    /// totality here as permission to omit an absent key.
    pub fn omit(&self, key: &str) -> RowType {
        RowType::new(
            self.columns
                .iter()
                .filter(|(n, _)| n != key)
                .cloned()
                .collect(),
        )
    }

    /// `prefix "s"` / `suffix "s"`: every name gains `affix` in front
    /// (`prefix`) or at the end. Reuses the validator's rule, so the checked
    /// layer and `schema.rs` cannot drift apart.
    pub fn rename_all(&self, affix: &str, prefix: bool) -> RowType {
        let names = schema::rename_columns(&self.names(), affix, prefix);
        RowType::new(names.into_iter().zip(self.columns.iter().map(|(_, t)| t.clone())).collect())
    }

    /// The **join** row former: the left row's columns in place, then the
    /// right row's that the left does not have. **Left wins on collision.**
    ///
    /// This is `rules::join_columns`, the same rule `schema::finish_schema`
    /// applies to `Rel::Join` and the checker applies to `Cons::JoinOut`. Use
    /// it for joins and for row *extension*.
    ///
    /// Do **not** use this for `update`: that is [`RowType::overwrite`], whose
    /// collision rule is the opposite. The two differ by one word and both
    /// "work", which is exactly why they are separate functions.
    pub fn merge(&self, right: &RowType) -> RowType {
        let out = rules::join_columns(&self.columns, &right.columns);
        RowType::new(out)
    }

    /// The **overwrite** row former used by `update` and by the `merge r s`
    /// row term: the left row's field *positions* are kept, a collision takes
    /// the **right** type, and right-only fields are appended.
    ///
    /// This is the checker's rule, not a guess:
    ///   * `check/reduce.rs` (`reduce_merge`, "Right-wins merge: keep left
    ///     field positions, replace collisions with right values, append right
    ///     fields not in left");
    ///   * `check/infer.rs` `Cons::Update`, which builds each output column as
    ///     `Some(v) => v` (the new value) and `None => old`, i.e. the
    ///     replacement wins.
    ///
    /// Contrast [`RowType::merge`], which is left-wins and belongs to joins.
    pub fn overwrite(&self, right: &RowType) -> RowType {
        let mut out = self.columns.clone();
        for (n, t) in &right.columns {
            match out.iter_mut().find(|(o, _)| o == n) {
                // The position is the left row's; the type is the right's.
                Some(slot) => slot.1 = t.clone(),
                None => out.push((n.clone(), t.clone())),
            }
        }
        RowType::new(out)
    }

    /// `mapValue (AsNullable)`: every field type wrapped in `maybe`.
    pub fn map_value_nullable(&self) -> RowType {
        RowType::new(
            self.columns
                .iter()
                .map(|(n, t)| (n.clone(), t.clone().nullable()))
                .collect(),
        )
    }

    /// The row a query exposes after a `select`/`agg`: exactly the listed
    /// fields, in the order they were written.
    ///
    /// This does not validate: it is the row *former*. Duplicate field names
    /// are rejected by the constructors that consume a field list (see
    /// [`duplicate`](RowType::duplicate)), and a caller that has not validated
    /// must call that first.
    pub fn project(fields: &[(String, ScalarType)]) -> RowType {
        RowType::new(fields.to_vec())
    }
}

/// A checked-layer error, tied to where it was found.
///
/// `message` is the diagnostic the schema layer would have produced for the
/// same program; the constructors keep the wording so a user sees one
/// explanation, not two.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub message: String,
    pub origin: Option<Origin>,
    pub def: Option<usize>,
    /// Who is at fault: the program, or this phase?
    ///
    /// The distinction matters at the production boundary
    /// (`eval::root_queries_checked`), which compares source elaboration against
    /// the evaluator and must react to the two differently:
    ///
    /// * [`Fault::Program`] — the program is malformed. `schema` would report
    ///   the same thing and the user should see that message. A `table` whose
    ///   defining definition declares no columns is an example: the query is
    ///   genuinely untypeable, and saying so is correct.
    /// * [`Fault::Compiler`] — the program is fine and this phase cannot build
    ///   it. The evaluator handles it; source elaboration does not. That is a
    ///   capability gap, and presenting it as the user's mistake would be a lie.
    ///
    /// Conflating the two is not academic. It is why `schema` could not simply
    /// be demoted: the elaborated path treats a column-less table as a *gap*
    /// while `schema` treats it as a *program* error, so with `schema` demoted
    /// to an assertion the compiler panicked on a program that deserves a plain
    /// diagnostic.
    pub fault: Fault,
}

/// Who is at fault for an [`Error`]. See its `fault` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The program is malformed; the message is for the user.
    Program,
    /// This phase cannot handle a program the evaluator can.
    Compiler,
}

impl Error {
    /// An error about the **program**: malformed, and the message is for the
    /// user. The default, because most constructor rejections are genuine rule
    /// violations — a column that is not grouped, set operands with different
    /// columns, an aggregate nested inside an aggregate.
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            origin: None,
            def: None,
            fault: Fault::Program,
        }
    }

    /// An error about **this phase**: the program is fine and the evaluator can
    /// build it, but the checked layer cannot. Used for missing capabilities,
    /// which the production boundary must not present as a user's mistake.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            origin: None,
            def: None,
            fault: Fault::Compiler,
        }
    }

    /// Whether this is a gap in this phase rather than a fault in the program.
    pub fn is_unsupported(&self) -> bool {
        self.fault == Fault::Compiler
    }

    /// Point the error at an origin, unless it already has one (the innermost
    /// node is the one that failed).
    pub fn at(mut self, origin: Origin) -> Self {
        if self.origin.is_none() {
            self.origin = Some(origin);
        }
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A diagnostic about one module, and optionally one definition of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub module: usize,
    pub def: Option<usize>,
    pub diag: Diag,
}

impl Diagnostic {
    pub fn new(module: usize, def: Option<usize>, diag: Diag) -> Self {
        Diagnostic { module, def, diag }
    }
}

/// A value a constructor has already validated.
///
/// The checked layer's invariant is "constructors check, consumers do not" —
/// so a `CheckedQuery` that exists has a row that matches its expression, and
/// the only way to make one is through a constructor. This wrapper marks the
/// arguments a constructor has checked, which is what keeps a consumer from
/// having to re-check them.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified<T>(T);

impl<T> Verified<T> {
    pub fn new(value: T) -> Self {
        Verified(value)
    }

    pub fn get(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

/// The scalar type a value of this template phase has.
///
/// A template's *kind* says which constructor builds it (`Value::Tpl`), and
/// the checked layer has a separate node for each; this maps the evaluator's
/// three-case enum onto that split, so a caller holding a `TplKind` knows
/// which checked constructor to call without matching on the enum itself.
pub fn template_node_kind(kind: TplKind) -> TemplateNodeKind {
    match kind {
        TplKind::Scalar => TemplateNodeKind::Scalar,
        TplKind::Agg => TemplateNodeKind::Aggregate,
        TplKind::Win => TemplateNodeKind::Window,
    }
}

/// Which checked expression node a template builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateNodeKind {
    /// `CheckedExprNode::Template`, phase from the arguments' mix.
    Scalar,
    /// `CheckedExprNode::AggTemplate`, `Agg` phase.
    Aggregate,
    /// `CheckedExprNode::WinTemplate`, `Win` phase, with a window spec.
    Window,
}

impl TemplateNodeKind {
    /// The phase this kind of template always produces. A scalar template's
    /// phase depends on its arguments and is not decided here.
    pub fn fixed_phase(self) -> Option<crate::ir::Phase> {
        match self {
            TemplateNodeKind::Scalar => None,
            TemplateNodeKind::Aggregate => Some(crate::ir::Phase::Agg),
            TemplateNodeKind::Window => Some(crate::ir::Phase::Win),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_row(names: &[&str]) -> RowType {
        RowType::new(names.iter().map(|n| (n.to_string(), ScalarType::Int)).collect())
    }

    #[test]
    fn origin_is_copy_eq_and_carries_module_and_span() {
        let o = Origin::new(2, Span { start: 5, end: 9 });
        let copy = o;
        assert_eq!(o, copy);
        assert_eq!(o.module, 2);
        assert_eq!((o.span.start, o.span.end), (5, 9));
        assert_ne!(o, Origin::new(3, Span { start: 5, end: 9 }));
    }

    #[test]
    fn scalar_types_print_the_way_signatures_spell_them() {
        assert_eq!(ScalarType::Int.to_string(), "int");
        assert_eq!(ScalarType::Maybe(Box::new(ScalarType::Int)).to_string(), "maybe int");
        assert_eq!(
            ScalarType::List(Box::new(ScalarType::String)).to_string(),
            "list string"
        );
        assert_eq!(ScalarType::Unknown.to_string(), "?");
    }

    #[test]
    fn nullable_is_idempotent_like_map_value_as_nullable() {
        let t = ScalarType::Int.nullable();
        assert_eq!(t, ScalarType::Maybe(Box::new(ScalarType::Int)));
        assert_eq!(t.clone().nullable(), t);
    }

    #[test]
    fn only_bool_is_a_condition() {
        assert!(ScalarType::Bool.is_bool());
        assert!(!ScalarType::Maybe(Box::new(ScalarType::Bool)).is_bool());
        assert!(!ScalarType::Int.is_bool());
    }

    #[test]
    fn merge_keeps_left_positions_and_appends_right_only_fields() {
        let left = RowType::new(vec![
            ("id".into(), ScalarType::Int),
            ("name".into(), ScalarType::String),
        ]);
        let right = RowType::new(vec![
            ("name".into(), ScalarType::Bool),
            ("age".into(), ScalarType::Int),
        ]);
        let merged = left.merge(&right);
        assert_eq!(
            merged.names(),
            vec!["id".to_string(), "name".to_string(), "age".to_string()]
        );
        // Left wins on collision.
        assert_eq!(merged.get("name"), Some(&ScalarType::String));
    }

    #[test]
    fn omit_drops_a_column_and_keeps_the_rest_in_order() {
        let row = int_row(&["a", "b", "c"]);
        assert_eq!(row.omit("b").names(), vec!["a".to_string(), "c".to_string()]);
        assert_eq!(row.omit("zzz"), row);
    }

    #[test]
    fn rename_all_matches_the_schema_rule() {
        let row = int_row(&["a", "b"]);
        assert_eq!(
            row.rename_all("u_", true).names(),
            vec!["u_a".to_string(), "u_b".to_string()]
        );
        assert_eq!(
            row.rename_all("_v2", false).names(),
            vec!["a_v2".to_string(), "b_v2".to_string()]
        );
        assert_eq!(row.rename_all("u_", true).get("u_a"), Some(&ScalarType::Int));
    }

    #[test]
    fn map_value_nullable_wraps_every_field_once() {
        let row = RowType::new(vec![
            ("a".into(), ScalarType::Int),
            ("b".into(), ScalarType::Maybe(Box::new(ScalarType::String))),
        ]);
        let mapped = row.map_value_nullable();
        assert_eq!(mapped.get("a"), Some(&ScalarType::Maybe(Box::new(ScalarType::Int))));
        // Already nullable: `maybe (maybe t) = maybe t`.
        assert_eq!(
            mapped.get("b"),
            Some(&ScalarType::Maybe(Box::new(ScalarType::String)))
        );
    }

    #[test]
    fn unknown_rows_carry_names_only() {
        let row = RowType::unknown(vec!["a".into(), "b".into()]);
        assert_eq!(row.names(), vec!["a".to_string(), "b".to_string()]);
        assert!(row.get("a").unwrap().is_unknown());
    }

    #[test]
    fn an_error_keeps_its_first_origin() {
        let e = Error::new("boom")
            .at(Origin::new(1, Span { start: 1, end: 2 }))
            .at(Origin::new(9, Span { start: 30, end: 40 }));
        assert_eq!(e.origin, Some(Origin::new(1, Span { start: 1, end: 2 })));
    }
}
