//! Source elaboration: build the checked tree directly from source.
//!
//! Type checking supplies the facts used to construct `CheckedQuery` directly
//! from source. The result is erased only at the SQL boundary.
//!
//! # What it reads, and what it does not compute
//!
//! It does **not** re-derive types. Everything it needs is recorded by the
//! checker and read through `TypeCheck`:
//!
//! * `use_ty(module, ExprId)` — the scalar type of one expression;
//! * `scheme_view` / `scheme_fields` — the row shape of a definition;
//! * `choices_of` — which overload candidate a use resolved to;
//! * `rules::is_pipe_name` and the prelude's stage declarations — which
//!   operator is a pipeline stage.
//!
//! Re-deriving any of those would create a second source of truth for types.
//!
//! # Scope
//!
//! Expressions are elaborated for the *scalar* cases a `CheckedExpr` can hold:
//! columns, literals, SQL templates, `in`, and `group`. An expression whose
//! type the checker did not record as a scalar — a query, a function, a nested
//! stage — is reported as an error rather than given an invented type, because
//! a `CheckedExpr` carrying a made-up `ScalarType` is exactly the kind of
//! plausible-but-wrong value this layer must not produce.

use crate::checked::CheckedQuery;
use crate::compile::CompilerInput;
use crate::core::{Error, Origin};
use cagara_syntax::ast::{self, ExprKind};

#[cfg(test)]
thread_local! {
    static ELABORATE_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ELABORATED_DEFS: std::cell::RefCell<Vec<(usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn elaborate_runs() -> usize {
    ELABORATE_RUNS.with(|runs| runs.get())
}

#[cfg(test)]
pub(crate) fn elaborated_defs() -> Vec<(usize, usize)> {
    ELABORATED_DEFS.with(|defs| defs.borrow().clone())
}

/// What elaborating one expression produced.
type R<T> = Result<T, Error>;

/// The marker a definition that is not a query fails with.
///
/// A sentinel rather than a real error: "this definition has no query to
/// elaborate" is a *finding*, not a failure, but `elaborate_module` returns one
/// `Result` per definition and a sentinel is the smallest way to add the third
/// outcome without changing that shape for every caller.
///
/// It is recognised by [`is_not_a_query`] rather than by comparing messages,
/// and its text is deliberately something no real diagnostic would say, so a
/// mistake shows up as strange output rather than as a silently swallowed
/// program error.
pub(crate) const NOT_A_QUERY: Error = Error {
    message: String::new(),
    origin: None,
    def: None,
    fault: crate::core::Fault::NotAQuery,
};

/// Whether an elaboration result means "this definition is not a query".
pub(crate) fn is_not_a_query(e: &Error) -> bool {
    e.fault == crate::core::Fault::NotAQuery
}

/// Elaborate the root module's definitions from source.
///
/// Returns one entry per definition, in source order, with the same
/// one result per root definition, in source order.
pub fn elaborate_module(input: CompilerInput<'_>, module: usize) -> Vec<(String, R<CheckedQuery>)> {
    let mut out = Vec::new();
    for (i, d) in input.module(module).source().defs.iter().enumerate() {
        let result = elaborate_definition(input, module, i, d);
        out.push((d.name.clone(), result));
    }
    out
}

fn validate_template_definition(module: usize, d: &ast::Def) -> R<()> {
    let ExprKind::Sql(sql) = &d.body.kind else {
        return Ok(());
    };
    let Some(ty) = d.ty.as_ref() else {
        return Ok(());
    };
    let arity = template_arity_from_type(ty, declared_kind(d));
    check_placeholders(sql, arity).map_err(|e| e.at(Origin::new(module, d.body.span)))
}

/// Elaborate one definition's body to a checked query.
fn elaborate_def(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    d: &ast::Def,
) -> R<CheckedQuery> {
    #[cfg(test)]
    ELABORATED_DEFS.with(|defs| defs.borrow_mut().push((module, def)));

    let tc = input.type_check();
    // A rejected definition has no trustworthy types. Its checker diagnostic
    // is emitted by the compilation boundary, so it must not be elaborated.
    if tc.error_for(module, def).is_some() {
        return Err(NOT_A_QUERY);
    }
    // A definition that leaves open overloads is meaningful only at its uses:
    // it has no body of its own, so there is nothing here to elaborate. This is
    // the same situation as "not a query": it has no query body to elaborate.
    if tc.holes(module, def) > 0 {
        return Err(NOT_A_QUERY);
    }
    // A definition that is **not a query** has nothing to elaborate here, and
    // that is not a failure. `bad : expr r int -> expr r int = sql "$0"` is a
    // function; building a `CheckedQuery` from it fails with "expected a query",
    // which is true and useless. A caller must be able to tell "not a query"
    // from "a query I could not build", because the production boundary reports
    // the second as a compiler gap and must stay silent about the first.
    //
    // Structural, on the scheme, rather than by catching error text:
    // `SchemeView::Query` is `query r`, and `Function`/`Scalar`/`Row` are
    // everything else.
    if !matches!(
        tc.scheme_view(module, def),
        Some(crate::check::SchemeView::Query { .. })
    ) {
        return Err(NOT_A_QUERY);
    }
    // The normalized route, and the only one — there is no source walk left to
    // fall back to.
    //
    // Normalization resolves every name, inlines every definition and reduces
    // every application, a stage's included, so by the time `lower_query` sees a
    // term there is no syntax left to re-read and no chain to follow. A
    // `Fault::Compiler` error here therefore means a construct reached the
    // language without a term rule, which is reported rather than handled by a
    // second implementation of the language that no longer exists.
    //
    // The scope, environment and hole assignment the source walk threaded
    // through its context are gone with it: normalization reads the checker's
    // records instead of re-resolving names, and reaches a callee's open
    // overloads through its own stack.
    crate::normalize::elaborate_query_via_terms(input, module, def, &d.body)
}

/// Elaborate one source definition for the compilation boundary.
pub(crate) fn elaborate_definition(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    definition: &ast::Def,
) -> R<CheckedQuery> {
    #[cfg(test)]
    ELABORATE_RUNS.with(|runs| runs.set(runs.get() + 1));
    validate_template_definition(module, definition)
        .and_then(|()| elaborate_def(input, module, def, definition))
}

/// How a `sql` template's result is *declared* to behave, read from its type
/// expression.
///
/// Deliberately not the resolved scheme — see the comment at the use site.
///
/// An alias rather than a second enum. Elaboration and normalization each had
/// their own three-case copy of this and their own reader of it, which is one
/// rule in two places: a declared head added to one side and not the other would
/// have been two silently different answers. The cases and the reader both live
/// in [`crate::normalize`] now, and this name is kept because it is what the
/// query path's call sites read as.
pub(crate) use crate::normalize::TemplateKind as DeclaredKind;

fn template_arity_from_type(t: &ast::TypeExpr, kind: DeclaredKind) -> usize {
    let mut n: usize = 0;
    let mut ty = t;
    while let ast::TypeExpr::Fun(_, r) = ty {
        n += 1;
        ty = r;
    }
    if kind == DeclaredKind::Win {
        n.saturating_sub(1)
    } else {
        n
    }
}

fn check_placeholders(sql: &str, expr_args: usize) -> R<()> {
    let bytes = sql.as_bytes();
    let mut seen = vec![false; expr_args + 1];
    let mut i = 0;
    let mut highest = 0;
    let mut in_string = false;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_string && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
            } else {
                in_string = !in_string;
                i += 1;
            }
            continue;
        }
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == i + 1 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: a `$` at byte {start} is not a placeholder"
            )));
        }
        let n = sql[i + 1..end]
            .parse::<usize>()
            .map_err(|_| Error::new("SQL template placeholder is too large"))?;
        let glued = |byte: u8| byte == b'_' || byte.is_ascii_alphanumeric() || byte == b'$';
        if in_string || bytes.get(end).copied().is_some_and(glued) || (i > 0 && glued(bytes[i - 1]))
        {
            return Err(Error::new(format!(
                "SQL template `{sql}`: `{}` must stand alone (not inside a name or string)",
                &sql[i..end]
            )));
        }
        if n == 0 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: placeholders are 1-based, so `${n}` is not an argument"
            )));
        }
        if n > expr_args {
            return Err(Error::new(format!(
                "template uses placeholders up to ${n} but its signature has {expr_args} expression argument(s)"
            )));
        }
        seen[n] = true;
        highest = highest.max(n);
        i = end;
    }
    if let Some(missing) = (1..=highest).find(|&n| !seen[n]) {
        return Err(Error::new(format!(
            "SQL template `{sql}` is missing placeholder `${missing}`"
        )));
    }
    if highest < expr_args {
        return Err(Error::new(format!(
            "template uses placeholders up to ${highest} but its signature has {expr_args} expression argument(s)"
        )));
    }
    Ok(())
}

/// The declared result kind of a `sql` template definition.
///
/// Thin, because the rule lives in one place now: this is the query path's name
/// for [`crate::normalize::declared_template_kind`], which the term layer also
/// reads. Two readers of one rule would be the duplication the migration exists
/// to remove.
fn declared_kind(d: &ast::Def) -> DeclaredKind {
    crate::normalize::declared_template_kind(d)
}
