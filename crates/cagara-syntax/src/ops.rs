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
//! special class of operator but the *level* the operator `&` is declared at.
//! An operator at that same fixity is a pipeline link, which is the only
//! reason `&+` behaves like `&` and not like `+`.
//!
//! Two sources fill the table:
//!
//! * The **built-ins** below — arithmetic, comparison, logic and
//!   concatenation. They are the language's own expression operators, and they
//!   are what the prelude itself is parsed with in order to read the prelude's
//!   declarations.
//! * Everything **declared in `prelude.cagara`** with `infixl` / `infixr`.
//!   That includes the query-syntax operators (the `&` pipeline family and the
//!   joins) and the function combinators `&`, `$` and `>>>`, whose fixity
//!   follows Haskell. The prelude is parsed once with the built-in table to
//!   collect the declarations, and the result is what the parser uses for
//!   everything else.
//!
//! That split is what makes a new stage shorthand a prelude edit rather than a
//! Rust edit: `infixl 1 &^` plus `_&^_ = q => fields => ...` is the whole
//! change. A declaration is read only from the prelude — the fixity of an
//! operator is a property of the language, not of one module — and
//! `cagara-hir` reports a declaration anywhere else.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The prelude *is* the language definition (it declares the operators and
/// defines their meaning), so this crate reads it. Every other use of the
/// prelude is in `cagara-hir`, which loads it as module 0.
pub const PRELUDE_SRC: &str = include_str!("../../../prelude.cagara");

/// Loosest precedence a declaration may name. `$` is declared here, so the
/// scale starts at zero like Haskell's.
pub const MIN_LEVEL: u8 = 0;

/// Tightest precedence a declaration may name. Binding powers are compared as
/// `u8` and a right-associative operator at level `n` uses `n + 1`, so the
/// level itself must leave room for that one.
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

/// The language's own expression operators: arithmetic, comparison, logic and
/// concatenation. Their fixity belongs to the language rather than to the
/// prelude's query syntax, so it is fixed here.
///
/// This list is also the *bootstrap*: the prelude is parsed with it alone in
/// order to read the prelude's own declarations, so anything a prelude body
/// uses has to be here. Today that is `*` (`n * 7`, `n * 3`). A test parses
/// the prelude against this table alone, so a body that reaches for an
/// operator missing from it fails loudly instead of silently dropping the
/// declarations.
///
/// The function combinators are deliberately absent: `&`, `$` and `>>>` are
/// defined in the prelude, so their fixity is declared beside them.
const BUILTIN: &[(&str, Fixity)] = &[
    ("||", Fixity::left(9)),  // or
    ("&&", Fixity::left(11)), // and
    ("==", Fixity::left(13)),
    ("!=", Fixity::left(13)),
    ("<", Fixity::left(13)),
    ("<=", Fixity::left(13)),
    (">", Fixity::left(13)),
    (">=", Fixity::left(13)),
    ("<>", Fixity::right(15)), // string concatenation
    ("+", Fixity::left(17)),
    ("-", Fixity::left(17)),
    ("*", Fixity::left(19)),
    ("/", Fixity::left(19)),
    ("%", Fixity::left(19)),
    ("??", Fixity::right(21)), // coalesce
];

/// The operator that *is* the pipeline. Its declared fixity defines the pipe
/// level, and an operator declared at that same fixity is a pipeline link like
/// it. This is the one name the core has to know, in the same way it knows
/// that `sql` introduces a template: `&` is the pipeline.
pub const PIPE: &str = "&";

/// Spelling to fixity.
#[derive(Debug, Clone)]
pub struct Ops {
    map: HashMap<String, Fixity>,
}

impl Ops {
    /// The built-in operators alone.
    pub fn builtin() -> Self {
        Ops {
            map: BUILTIN.iter().map(|(s, f)| (s.to_string(), *f)).collect(),
        }
    }

    /// The built-ins, plus whatever `src` declares. Applying the declarations
    /// over the built-ins means a `sql` template or an expression operator can
    /// be re-fixed (`infixr 19 *`) as well as a shorthand added.
    pub fn with_declarations(src: &str) -> Self {
        let mut ops = Ops::builtin();
        // Parsed with the built-in table only: declarations are read from a
        // source that cannot use the operators it declares. The prelude's
        // bodies use arithmetic and comparison, never the pipeline, so one
        // pass is enough. (`parse` would consult the global table, which is
        // what this function is building.)
        let (module, _errors) = crate::ast::lower_with(src, &ops);
        for d in &module.operators {
            ops.map.insert(d.spelling.clone(), d.fixity);
        }
        ops
    }

    /// The fixity of an operator, by spelling. Every infix operator is one of
    /// these, `&` and `&+` included: they differ only in precedence and
    /// associativity.
    pub fn find(&self, spelling: &str) -> Option<Fixity> {
        self.map.get(spelling).copied()
    }

    /// The pipe level: whatever fixity `&` is declared with. No operator has a
    /// "stage" mark of its own — an operator is a pipeline link exactly when
    /// it sits where the pipeline sits.
    pub fn pipe_fixity(&self) -> Option<Fixity> {
        self.find(PIPE)
    }

    /// Does `spelling` build a pipeline stage? True for `&` itself and for
    /// every operator declared at the same precedence and associativity —
    /// `&?`, `&=`, `&+`, …, but not the joins at their own looser level.
    ///
    /// Three things follow from this and nothing else has to declare them: the
    /// ten-stage pipeline budget, the formatter's one-stage-per-line rule, and
    /// locating a diagnostic at the stage that failed rather than at the whole
    /// pipeline.
    pub fn is_stage(&self, spelling: &str) -> bool {
        match (self.find(spelling), self.pipe_fixity()) {
            (Some(f), Some(pipe)) => f == pipe,
            _ => false,
        }
    }

    /// Is `name` (an `_op_` definition name) a stage operator? Used by the
    /// evaluator to recognise `q & stage` through its desugared call.
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

/// The operator table of the language as compiled in: the built-ins plus the
/// declarations in `prelude.cagara`.
///
/// Built once. An operator's fixity is global, so this is not a per-module
/// setting: declarations in any other module are reported rather than
/// honoured (see `cagara-hir`'s `file_diags`).
pub fn ops() -> &'static Ops {
    static OPS: OnceLock<Ops> = OnceLock::new();
    OPS.get_or_init(|| Ops::with_declarations(PRELUDE_SRC))
}

/// The name a spelling desugars to: `+` is `_+_`, `&?` is `_&?_`.
pub fn op_name(spelling: &str) -> String {
    format!("_{spelling}_")
}

/// The spelling an operator name stands for, when it is one.
pub fn op_spelling(name: &str) -> Option<&str> {
    name.strip_prefix('_')?
        .strip_suffix('_')
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_the_documented_fixity() {
        let ops = Ops::builtin();
        // Left-assoc, right-assoc, and the tightest expression operator.
        assert_eq!(ops.find("+").unwrap().precedence(), (17, 18));
        assert_eq!(ops.find("??").unwrap().precedence(), (22, 21));
        assert_eq!(ops.find("<>").unwrap().precedence(), (16, 15));
        // `+` is an ordinary operator: nothing sits at level 1 yet, so nothing
        // is a stage.
        assert!(!ops.is_stage("+"));
        assert_eq!(ops.pipe_fixity(), None);
        // The combinators are declared in the prelude, not here.
        assert!(ops.find("&").is_none());
        assert!(ops.find("$").is_none());
        assert!(ops.find(">>>").is_none());
    }

    /// The built-in table is the bootstrap: the prelude is parsed with it alone
    /// in order to read the prelude's own declarations. A body that reaches for
    /// an operator missing from it would silently produce no declarations at
    /// all, so this is a test rather than a comment.
    #[test]
    fn the_prelude_parses_with_the_builtin_table_alone() {
        let (module, errors) = crate::ast::lower_with(PRELUDE_SRC, &Ops::builtin());
        assert!(errors.is_empty(), "{errors:?}");
        // And the declarations really are readable that way.
        assert!(!module.operators.is_empty());
    }

    /// The function combinators carry Haskell's fixities: `$` infixr 0, `&`
    /// infixl 1 (`Data.Function`), and the two directions of composition at
    /// infixr 23, tighter than every other operator the way `(.)` is in
    /// Haskell.
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

    /// The compiled-in table is the built-ins plus the prelude's declarations,
    /// and the pipeline is the level `&` sits at.
    #[test]
    fn the_prelude_declares_the_pipeline() {
        let ops = ops();
        assert_eq!(ops.pipe_fixity().unwrap().precedence(), (1, 2));
        for s in ["&", "&?", "&=", "&+", "&*", "&.", "&-"] {
            let f = ops.find(s).unwrap_or_else(|| panic!("`{s}` is undeclared"));
            assert_eq!(f.precedence(), (1, 2), "`{s}` must sit at the pipe level");
            assert!(ops.is_stage(s), "`{s}` must be a stage");
        }
        // The joins are declared too, at their own level, so they are not
        // stages despite building a query.
        for s in ["?", "<?", "?>", "<?>"] {
            let f = ops.find(s).unwrap_or_else(|| panic!("`{s}` is undeclared"));
            assert_eq!(f.precedence(), (3, 4));
            assert!(!ops.is_stage(s), "`{s}` is a join, not a stage");
        }
        assert_eq!(ops.stages(), ["&", "&*", "&+", "&-", "&.", "&=", "&?"]);
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

    /// A declaration may re-fix a built-in, and there is only one form: the
    /// level decides everything, including whether the operator is a stage.
    /// `&` has to be declared for there to be a pipe level to compare against.
    #[test]
    fn declarations_apply_over_the_builtins() {
        let src = "infixl 1 &\ninfixr 19 *\ninfixl 1 &^\ninfixl 3 ?\n";
        let ops = Ops::with_declarations(src);
        assert_eq!(ops.find("*").unwrap().precedence(), (20, 19));
        assert_eq!(ops.find("&^").unwrap().precedence(), (1, 2));
        assert_eq!(ops.find("?").unwrap().precedence(), (3, 4));
        // Only the operators at `&`'s level are stages.
        assert!(ops.is_stage("&"));
        assert!(ops.is_stage("&^"));
        assert!(!ops.is_stage("?"));
        assert!(!ops.is_stage("*"));
        assert!(!ops.is_stage("$"));
    }

    /// Being a stage is nothing but sharing `&`'s precedence *and*
    /// associativity: move an operator off that level, or flip its
    /// associativity, and it is an ordinary operator again. There is no mark
    /// to keep in step with the level.
    #[test]
    fn being_a_stage_is_only_a_question_of_where_you_sit() {
        let cases = [
            ("infixl 1 &^\n", true),
            ("infixl 2 &^\n", false),
            ("infixr 1 &^\n", false),
            ("infixl 3 &^\n", false),
        ];
        for (decl, want) in cases {
            // A `&` to define the pipe level, plus the operator under test.
            let ops = Ops::with_declarations(&format!("infixl 1 &\n{decl}"));
            assert_eq!(ops.is_stage("&^"), want, "{decl:?}");
        }
    }

    /// Every built-in spelling must survive lexing as a single token, or the
    /// fixity here would never be consulted. The lexer's named tokens and this
    /// table are two lists of the same fact, so pin them together.
    #[test]
    fn every_builtin_spelling_is_one_lexed_token() {
        for (s, _) in BUILTIN {
            let mut lex = crate::lexer::lex(s)
                .into_iter()
                .filter(|l| !l.kind.is_trivia());
            let first = lex.next().unwrap_or_else(|| panic!("`{s}` did not lex"));
            assert_eq!(first.text, *s, "`{s}` lexes as more than one token");
            assert!(lex.next().is_none(), "`{s}` lexes as more than one token");
        }
    }

    /// A declaration the parser rejects must not take effect either: the
    /// syntax error already names the problem, and a stray operator on top of
    /// it would be a second, silent one.
    #[test]
    fn a_rejected_declaration_does_not_take_effect() {
        for src in [
            "infixl 251 &^\n",
            "infixl 1\n",
            "infixl &^\n",
            // `stage` is not a declaration form at all now, so it is just an
            // ordinary name with a missing `=`.
            "stage &^\n",
        ] {
            let ops = Ops::with_declarations(src);
            assert!(ops.find("&^").is_none(), "{src:?} declared `&^`");
            // The built-ins are untouched.
            assert_eq!(ops.find("+").unwrap().precedence(), (17, 18), "{src:?}");
        }
        // Level 0 is a real level — `$` is declared there.
        let ops = Ops::with_declarations("infixl 1 &\ninfixl 0 &^\n");
        assert_eq!(ops.find("&^").unwrap().precedence(), (0, 1));
        assert!(!ops.is_stage("&^"), "level 0 is not the pipe level");
    }
}
