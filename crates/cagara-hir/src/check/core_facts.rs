//! Facts the checker owns and every later phase reads.
//!
//! Two types used to live in [`crate::normalize`], the module that holds the
//! normalized term calculus: [`Resolved`] — what a name occurrence resolved to —
//! and [`TemplateKind`] — the declared result shape of a `sql` template. Neither
//! is a fact *about* normalization. Name resolution records `Resolved`, the
//! checker records `TemplateKind` from a definition's declared signature, and
//! both are read by name resolution, inference, elaboration, the checked layer,
//! and the production normalizer alike.
//!
//! They live here next to the checker that produces them; [`crate::normalize`]
//! re-exports them so later phases read the same definitions rather than copies.

use cagara_syntax::ast;

/// A definition's identity: module index and source-order position.
pub type DefId = (usize, usize);

/// Which checked constructor a `sql` template lowers to.
///
/// Read from the template's **declared** signature, not from its resolved
/// scheme: `upper : expr r string -> expr r string` is a scalar template,
/// `sum : expr r a -> agg (expr r a)` an aggregate, `rowNumber : winspec r a ->
/// win (expr r a)` a window function. The declared form is the only place that
/// distinction survives, because once a template is applied its scheme says
/// what it *returned*, not which node built it.
///
/// One enum with one reader (`declared_template_kind`) on purpose. A second
/// copy of this three-case enum already existed once — every reader built its
/// checked node from a `kind` read off the same three strings — and a `head`
/// added to one copy and not the other would have been two silently different
/// answers rather than a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    /// A scalar template: `expr r a`.
    Scalar,
    /// An aggregate template: `agg (expr r a)`.
    Agg,
    /// A window function: `win (expr r a)`.
    Win,
}

/// What a name was resolved to.
///
/// This is the input side of normalization: a caller that has already asked the
/// checker what a name means hands the answer to the normalizer, which reduces
/// the definition. Keeping it separate is what makes aliases disappear without
/// a `follow_alias` walk — an alias *is*
/// its target, so the second lookup simply does not happen.
///
/// Mirrors [`crate::workspace::Binding`] rather than reusing it, for two
/// reasons: `Binding` is the *resolver's* vocabulary and includes cases (a
/// module, a name bound to nothing) that cannot denote a term, and a consumer
/// should not gain a new variant every time the resolver does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The checker resolved the name to this definition.
    Def(DefId),
    /// The name is defined more than once with signatures, and the checker
    /// picked one candidate **per use**. The candidates travel with the
    /// resolution because which one applies is a fact about *this* occurrence —
    /// recorded in `TypeCheck`'s choices — not a property of the name.
    Overloads { module: usize, defs: Vec<usize> },
    /// The checker resolved the name to a primitive.
    Primitive,
}

impl Resolved {
    /// The resolution a module-scope binding denotes, or `None` when the
    /// binding cannot denote a term at all.
    ///
    /// `None` for [`crate::workspace::Binding::Module`]: a module is not a
    /// value, and a bare reference to one is already a checker error, so
    /// there is nothing to report here.
    pub fn of_binding(binding: &crate::workspace::Binding) -> Option<Self> {
        match binding {
            crate::workspace::Binding::Def(m, d) => Some(Resolved::Def((*m, *d))),
            crate::workspace::Binding::Overloads(m, defs) => Some(Resolved::Overloads {
                module: *m,
                defs: defs.clone(),
            }),
            crate::workspace::Binding::Module(_) => None,
        }
    }
}

/// The phase kind a definition's **declared signature** gives it.
///
/// The single reader of this rule. Elaboration used to carry the same
/// three-case match over the same three strings, with its own copy of the enum,
/// justified by an analogy to `lit_of` — but the two are not alike. `lit_of`
/// translates between two `Lit` enums that *should* stay independent, so
/// writing it out makes a new AST variant a compile error there. This reads
/// three `head` strings out of a type expression and answers a question both
/// layers ask; a fourth kind added to one side and not the other would have
/// been two silently different answers, not a compile error.
///
/// Read from the type expression rather than the resolved scheme because only
/// the declared form distinguishes `expr r a` from `agg (expr r a)`; after
/// resolution both are just the result type.
pub fn declared_template_kind(d: &ast::Def) -> TemplateKind {
    use ast::TypeExpr;
    let Some(mut t) = d.ty.as_ref() else {
        return TemplateKind::Scalar;
    };
    // Strip the argument arrows to reach the result.
    while let TypeExpr::Fun(_, r) = t {
        t = r;
    }
    match t {
        TypeExpr::App { head, .. } if head == "agg" => TemplateKind::Agg,
        TypeExpr::App { head, .. } if head == "win" => TemplateKind::Win,
        _ => TemplateKind::Scalar,
    }
}
