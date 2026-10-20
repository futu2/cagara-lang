//! The infix operator table: every operator's spelling and fixity.
//!
//! An operator is a *spelling* (`+`, `&?`, `~>`). The lexer produces the
//! spelling and this table says what the parser should do with it. Meaning is
//! never here: `a + b` desugars to the application `_+_ a b`, and `_+_` is an
//! ordinary definition in `prelude.cagara`. So the core knows spellings and
//! precedence and nothing else.
//!
//! There is one kind of thing here. `+`, `&` and `&+` are all infix operators
//! and differ only in precedence and associativity; the pipeline is not a
//! special class of operator but the *level* of `&`. An operator at that same
//! fixity is a pipeline link, which is the only reason `&+` behaves like `&`
//! and not like `+`.
//!
//! The table is **fixed at build time**. Source cannot add an operator or
//! change one's precedence: `infixl` and `infixr` are not part of the language.
//! The prelude defines what the operators *mean* (`_&^_` is `intersect`), but
//! the spelling and fixity are the compiler's. That split is deliberate: every
//! file is parsed with the same grammar whether or not it imports the prelude.
//! A module can opt out of the names and still write `1 + 2` — the operator
//! parses, and `_+_` is a name it has not defined.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The prelude defines the language's *meanings* (`_+_`, `_&^_`, …), so this
/// crate reads it to check that the table and the definitions agree. Every
/// other use of the prelude is in `cagara-hir`, which loads it as module 0.
pub const PRELUDE_SRC: &str = include_str!("../../../prelude.cagara");

/// Loosest level used by the table. `$` sits here, so the scale starts at
/// zero like Haskell's.
pub const MIN_LEVEL: u8 = 0;

/// Tightest level the table could use. Binding powers are compared as `u8` and
/// an operator at level `n` spends `n + 1` on one of its two binding powers,
/// so a level must leave room for that increment; `MAX_LEVEL + 1` still fits
/// in `u8`.
pub const MAX_LEVEL: u8 = 250;

/// Binding powers of one operator: the tightness required of its left and
/// right operands. A left-associative operator at level `n` binds its left
/// operand at `n` and its right at `n + 1` (so `a - b - c` is `(a - b) - c`);
/// a right-associative one mirrors that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fixity {
    pub l_bp: u8,
    pub r_bp: u8,
}

impl Fixity {
    pub const fn left(level: u8) -> Self {
        Fixity {
            l_bp: level,
            r_bp: level + 1,
        }
    }

    pub const fn right(level: u8) -> Self {
        Fixity {
            l_bp: level + 1,
            r_bp: level,
        }
    }

    pub fn precedence(self) -> (u8, u8) {
        (self.l_bp, self.r_bp)
    }
}

/// The language's operators and their fixity.
///
/// Fixity is a **parsing** fact: `1 + 2 * 3` has to group as `1 + (2 * 3)`
/// before any name is resolved, so the table must be known before a file is
/// read and cannot come from a module. Meaning is a *resolution* fact:
/// `1 + 2` desugars to `_+_ 1 2`, and `_+_` is an ordinary definition. A
/// module that does not import one — or the prelude, which has it — gets an
/// unknown name, exactly as it would for any other function.
///
/// Keeping the two apart is what lets a file opt out of the prelude with the
/// grammar intact.
///
/// The list is fixed: `infixl` / `infixr` are not part of Cagara source, so
/// nothing a module writes can add to or re-fix it. Defining `_+_` gives `+`
/// a meaning; it does not give it a precedence.
///
/// Ordering here is for reading only; [`Ops::find`] is the lookup.
const FIXITY: &[(&str, Fixity)] = &[
    // Loosest: function application, reversed.
    ("$", Fixity::right(0)),
    // The pipeline, and every stage shorthand beside it. An operator is a
    // stage because it sits at `&`'s level, so adding one here is what makes
    // `&^` a stage rather than an ordinary operator.
    ("&", Fixity::left(1)),
    ("&!", Fixity::left(1)),
    ("&*", Fixity::left(1)),
    ("&+", Fixity::left(1)),
    ("&-", Fixity::left(1)),
    ("&.", Fixity::left(1)),
    ("&=", Fixity::left(1)),
    ("&?", Fixity::left(1)),
    ("&^", Fixity::left(1)),
    ("&|", Fixity::left(1)),
    ("&~", Fixity::left(1)),
    // Joins, between the pipeline and every expression operator.
    ("?", Fixity::left(3)),
    ("<?", Fixity::left(3)),
    ("?>", Fixity::left(3)),
    ("<?>", Fixity::left(3)),
    // Logic.
    ("||", Fixity::left(9)),
    ("&&", Fixity::left(11)),
    // Comparison.
    ("==", Fixity::left(13)),
    ("!=", Fixity::left(13)),
    ("<", Fixity::left(13)),
    ("<=", Fixity::left(13)),
    (">", Fixity::left(13)),
    (">=", Fixity::left(13)),
    ("<>", Fixity::right(15)), // string concatenation
    // Arithmetic.
    ("+", Fixity::left(17)),
    ("-", Fixity::left(17)),
    ("*", Fixity::left(19)),
    ("/", Fixity::left(19)),
    ("%", Fixity::left(19)),
    // Null coalescing: `x ?? default`.
    ("??", Fixity::right(21)),
    // Tightest: composition, both directions.
    (">>>", Fixity::right(23)),
    ("<<<", Fixity::right(23)),
];

/// The operator that *is* the pipeline. Its fixity defines the pipe level, and
/// an operator at that same fixity is a pipeline link like it. This is the one
/// name the core has to know, in the same way it knows that `sql` introduces a
/// template: `&` is the pipeline.
pub(crate) const PIPE: &str = "&";

/// Spelling to fixity.
#[derive(Debug, Clone)]
pub struct Ops {
    map: HashMap<String, Fixity>,
}

impl Default for Ops {
    /// The language's operator table. There is exactly one, fixed at build
    /// time: nothing in a source file can add to it or re-fix it.
    ///
    /// A `Default` rather than a `new`, because there is nothing to choose —
    /// the table *is* the language, and an empty or partial one is not a
    /// meaningful thing to ask for.
    fn default() -> Self {
        Ops {
            map: FIXITY.iter().map(|(s, f)| (s.to_string(), *f)).collect(),
        }
    }
}

impl Ops {
    /// The fixity of an operator, by spelling. Every infix operator is one of
    /// these, `&` and `&+` included: they differ only in precedence and
    /// associativity.
    pub fn find(&self, spelling: &str) -> Option<Fixity> {
        self.map.get(spelling).copied()
    }

    /// The pipe level: the fixed fixity of `&`. No operator has a "stage"
    /// mark of its own — an operator is a pipeline link exactly when it sits
    /// where the pipeline sits.
    pub fn pipe_fixity(&self) -> Option<Fixity> {
        self.find(PIPE)
    }

    /// Does `spelling` build a pipeline stage? True for `&` itself and for
    /// every operator at the same precedence and associativity —
    /// `&?`, `&=`, `&+`, …, but not the joins at their own looser level.
    ///
    /// Two things follow from this and nothing else has to declare them: the
    /// formatter's one-stage-per-line rule and
    /// locating a diagnostic at the stage that failed rather than at the whole
    /// pipeline.
    pub fn is_stage(&self, spelling: &str) -> bool {
        match (self.find(spelling), self.pipe_fixity()) {
            (Some(f), Some(pipe)) => f == pipe,
            _ => false,
        }
    }

    /// Is `name` (an `_op_` definition name) a stage operator? Source
    /// elaboration uses it to recognise `q & stage` through its desugared call.
    pub fn is_stage_name(&self, name: &str) -> bool {
        op_spelling(name).is_some_and(|s| self.is_stage(s))
    }

    /// Every stage operator's spelling, sorted, for tests and diagnostics.
    pub fn stages(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .map
            .keys()
            .map(String::as_str)
            .filter(|s| self.is_stage(s))
            .collect();
        out.sort_unstable();
        out
    }

    /// Every known spelling, sorted.
    pub fn spellings(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.map.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }
}

/// The operator table of the language.
///
/// Fixed, so every file is parsed with the same grammar whether or not it
/// imports the prelude. That is what lets a module opt out of the prelude and
/// still write `1 + 2` — the operators parse, and `_+_` is simply a name it
/// has not defined, like any other.
pub fn ops() -> &'static Ops {
    static OPS: OnceLock<Ops> = OnceLock::new();
    OPS.get_or_init(Ops::default)
}

/// The name a spelling desugars to: `+` is `_+_`, `&?` is `_&?_`.
pub fn op_name(spelling: &str) -> String {
    format!("_{spelling}_")
}

/// The spelling an operator name stands for, when it is one.
pub(crate) fn op_spelling(name: &str) -> Option<&str> {
    name.strip_prefix('_')?
        .strip_suffix('_')
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    // Test code may assert with `unwrap`, `expect`, and indexing: a panic
    // here is a failing test, not a broken compiler.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn operators_have_the_documented_fixity() {
        let ops = Ops::default();
        // Left-assoc, right-assoc, and the tightest expression operator.
        assert_eq!(ops.find("+").unwrap().precedence(), (17, 18));
        assert_eq!(ops.find("??").unwrap().precedence(), (22, 21));
        assert_eq!(ops.find("<>").unwrap().precedence(), (16, 15));
        // `+` is an ordinary operator, not a stage.
        assert!(!ops.is_stage("+"));
    }

    /// The prelude parses against the fixed table, and its bodies use nothing
    /// the table does not have.
    ///
    /// The table is fixed, so a prelude body that reaches for a spelling it
    /// does not contain fails to parse rather than silently declaring
    /// nothing. This is the test that says so.
    #[test]
    fn the_prelude_parses_against_the_fixed_table() {
        let (_module, errors) = crate::ast::lower_with(PRELUDE_SRC, &Ops::default());
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// The function combinators carry Haskell's fixities: `$` is right at
    /// level 0, `&` is left at level 1 (`Data.Function`), and the two
    /// directions of composition are right at level 23, tighter than every
    /// other operator the way `(.)` is in Haskell.
    #[test]
    fn the_combinators_follow_haskell() {
        let ops = ops();
        assert_eq!(ops.find("$").unwrap().precedence(), (1, 0));
        assert_eq!(ops.find("&").unwrap().precedence(), (1, 2));
        assert_eq!(ops.find(">>>").unwrap().precedence(), (24, 23));
        // The two directions of composition are a mirror pair: same fixity.
        assert_eq!(
            ops.find("<<<").unwrap(),
            ops.find(">>>").unwrap(),
            "`>>>` and `<<<` are the same operator in two directions"
        );
        // `$` is the loosest operator there is, application excluded.
        for s in ops.spellings() {
            if s != "$" {
                assert!(
                    ops.find(s).unwrap().l_bp >= ops.find("$").unwrap().l_bp,
                    "`{s}` is looser than `$`"
                );
            }
        }
        // Composition is the tightest, so `f >>> g $ x` composes first, and
        // `a & f >>> g` composes inside the stage.
        assert!(ops.find(">>>").unwrap().l_bp > ops.find("??").unwrap().l_bp);
        assert!(ops.find(">>>").unwrap().l_bp > ops.find("*").unwrap().l_bp);
        assert!(ops.find(">>>").unwrap().l_bp > ops.find("==").unwrap().l_bp);
        // None of them is a stage: only `&`'s exact fixity is.
        assert!(ops.is_stage("&"));
        assert!(!ops.is_stage("$"));
        assert!(!ops.is_stage(">>>"));
        assert!(!ops.is_stage("<<<"));
    }

    /// The fixed table contains the pipeline and every stage shorthand, all
    /// at the level `&` sits at.
    #[test]
    fn the_fixed_table_places_the_pipeline() {
        let ops = ops();
        assert_eq!(ops.pipe_fixity().unwrap().precedence(), (1, 2));
        for s in [
            "&", "&?", "&=", "&+", "&*", "&.", "&-", "&|", "&!", "&^", "&~",
        ] {
            let f = ops.find(s).unwrap_or_else(|| panic!("`{s}` is undeclared"));
            assert_eq!(f.precedence(), (1, 2), "`{s}` must sit at the pipe level");
            assert!(ops.is_stage(s), "`{s}` must be a stage");
        }
        // The joins are in the table at their own level, so they are not
        // stages despite building a query.
        for s in ["?", "<?", "?>", "<?>"] {
            let f = ops.find(s).unwrap_or_else(|| panic!("`{s}` is undeclared"));
            assert_eq!(f.precedence(), (3, 4));
            assert!(!ops.is_stage(s), "`{s}` is a join, not a stage");
        }
        // Every operator at the pipe level is a stage, and this list is the
        // whole of them. Keeping it exact is the point: the stage set is what
        // source elaboration uses to recognise `q & stage`, so a new entry at
        // this level changes how a pipeline is located for diagnostics.
        assert_eq!(
            ops.stages(),
            ["&", "&!", "&*", "&+", "&-", "&.", "&=", "&?", "&^", "&|", "&~"]
        );
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(op_name("&?"), "_&?_");
        assert_eq!(op_spelling("_&?_"), Some("&?"));
        assert_eq!(op_spelling("_+_"), Some("+"));
        assert_eq!(op_spelling("_$_"), Some("$"));
        assert_eq!(op_spelling("foo"), None);
        assert_eq!(op_spelling("__"), None);
        assert_eq!(op_spelling("_"), None);
    }

    /// A stage is nothing but an operator that sits at the pipe level.
    ///
    /// The level is fixed now, so this asserts the table's own shape: exactly
    /// the operators written at `&`'s precedence are stages, and the joins --
    /// which are also part of the query language -- are not, because they sit
    /// at their own looser level.
    #[test]
    fn being_a_stage_is_only_a_question_of_where_you_sit() {
        let ops = Ops::default();
        let pipe = ops.pipe_fixity().expect("`&` is the pipeline");
        for s in ops.spellings() {
            assert_eq!(
                ops.is_stage(s),
                ops.find(s) == Some(pipe),
                "`{s}` must be a stage exactly when it sits at the pipe level"
            );
        }
        assert!(ops.is_stage("&"));
        assert!(!ops.is_stage("?"));
        assert!(!ops.is_stage("*"));
        assert!(!ops.is_stage("$"));
    }

    /// Every operator spelling must survive lexing as a single token, or the
    /// fixity here would never be consulted. The lexer's named tokens and this
    /// table are two lists of the same fact, so pin them together.
    #[test]
    fn every_builtin_spelling_is_one_lexed_token() {
        for (s, _) in FIXITY {
            let mut lex = crate::lexer::lex(s)
                .into_iter()
                .filter(|l| !l.kind.is_trivia());
            let first = lex.next().unwrap_or_else(|| panic!("`{s}` did not lex"));
            assert_eq!(first.text, *s, "`{s}` lexes as more than one token");
            assert!(lex.next().is_none(), "`{s}` lexes as more than one token");
        }
    }

    /// A qualified name reaches everything an unqualified one can, operators
    /// included.
    ///
    /// An operator is defined as `_+_`, so `m._+_` is an ordinary qualified
    /// name and has to lex as one. It did not: `Field` matched only word
    /// characters after the dot, so `m._+_` lexed as `m` `.` `_` `+` `_` and
    /// reported a definition `_` that does not exist — which made a user
    /// prelude impossible to import by alias, the one thing `# no prelude` is
    /// for.
    ///
    /// `<` and `>` are the delicate case, being both operator characters and
    /// the start of the join-side fields. The two differ at the character
    /// after the dot, and this pins that they stay apart.
    #[test]
    fn a_qualified_name_reaches_operators() {
        let kinds = |s: &str| -> Vec<(crate::lexer::Token, std::string::String)> {
            crate::lexer::lex(s)
                .into_iter()
                .filter(|l| !l.kind.is_trivia())
                .map(|l| (l.kind, l.text.to_string()))
                .collect()
        };
        use crate::lexer::Token::*;
        // An operator name after a dot is one `Field`, not `Field` + operator.
        for (src, op) in [("m._+_", "._+_"), ("m._>_", "._>_"), ("m._<_", "._<_")] {
            assert_eq!(
                kinds(src),
                vec![(Ident, "m".into()), (Field, op.into())],
                "{src} must be a qualified name, not a field and an operator"
            );
        }
        // The join-side fields still win their own spellings.
        for (src, tok, text) in [
            (".<k", LeftField, ".<k"),
            (".>k", RightField, ".>k"),
            (".k", Field, ".k"),
        ] {
            assert_eq!(kinds(src), vec![(tok, text.into())], "{src}");
        }
    }

    /// `SyntaxKind::is_op_symbol` is what makes the parser look up a fixity
    /// instead of reporting an unexpected token, and it is a third list of the
    /// operator spellings. Every spelling this table knows must lex to a token
    /// that answers `true`, or it would be in the table yet unusable.
    #[test]
    fn every_known_operator_spelling_lexes_to_an_op_symbol() {
        for spelling in ops().spellings() {
            let first = crate::lexer::lex(spelling)
                .into_iter()
                .find(|l| !l.kind.is_trivia())
                .unwrap_or_else(|| panic!("`{spelling}` lexed to nothing"));
            assert!(
                first.kind.is_op_symbol(),
                "`{spelling}` lexes to {:?}, which is not classified as an op symbol",
                first.kind
            );
        }
        // And the classification is not vacuously true.
        for spelling in ["x", "1", "\"s\"", "(", "{", "]"] {
            let first = crate::lexer::lex(spelling).into_iter().next().unwrap();
            assert!(
                !first.kind.is_op_symbol(),
                "`{spelling}` must not be an op symbol"
            );
        }
    }

    /// The tightest operator the language allows still has room for the `+ 1`
    /// that associativity spends. `MAX_LEVEL` bounds the *level*; the largest
    /// binding power it can produce is `MAX_LEVEL + 1`.
    #[test]
    fn the_maximum_level_leaves_room_for_its_binding_power() {
        assert_eq!(
            Fixity::left(MAX_LEVEL).precedence(),
            (MAX_LEVEL, MAX_LEVEL + 1)
        );
        assert_eq!(
            Fixity::right(MAX_LEVEL).precedence(),
            (MAX_LEVEL + 1, MAX_LEVEL)
        );
    }

    /// Fixity is not declarable: `infixl` and `infixr` are gone from the
    /// language, so a line that looks like a declaration is an ordinary
    /// definition that happens to be missing its `=`.
    ///
    /// This is the invariant that makes the table fixed — and therefore what
    /// lets a module parse identically whether or not it imports the prelude.
    #[test]
    fn fixity_cannot_be_declared_in_source() {
        // The words are ordinary names, not keywords, so a line beginning with
        // one parses as a definition.
        let (_module, errors) = crate::ast::lower_with("infixl = 1\n", &Ops::default());
        assert!(errors.is_empty(), "{errors:?}");
    }
}
