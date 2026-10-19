//! Shared checks at the boundary between type checking and normalization.
//!
//! The production compiler consumes the checker's facts directly in the
//! normalizer. This module only keeps the boundary rules that must run before
//! normalization: template placeholder validation and query-shape checks.

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

/// Record that a root definition reached the normalization boundary.
pub(crate) fn record_definition(module: usize, def: usize) {
    #[cfg(test)]
    {
        ELABORATED_DEFS.with(|defs| defs.borrow_mut().push((module, def)));
        ELABORATE_RUNS.with(|runs| runs.set(runs.get() + 1));
    }
    #[cfg(not(test))]
    let _ = (module, def);
}

type Result<T> = std::result::Result<T, Error>;

pub(crate) const NOT_A_QUERY: Error = Error {
    message: String::new(),
    origin: None,
    def: None,
    fault: crate::core::Fault::NotAQuery,
};

pub(crate) fn is_not_a_query(error: &Error) -> bool {
    error.fault == crate::core::Fault::NotAQuery
}

/// Check the facts needed before reducing one definition.
pub(crate) fn prepare_query_definition(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    definition: &ast::Def,
) -> Result<()> {
    validate_template_definition(module, definition)?;
    let tc = input.type_check();
    if tc.error_for(module, def).is_some() || tc.holes(module, def) > 0 {
        return Err(NOT_A_QUERY);
    }
    if !matches!(
        tc.scheme_view(module, def),
        Some(crate::check::SchemeView::Query { .. })
    ) {
        return Err(NOT_A_QUERY);
    }
    Ok(())
}

fn validate_template_definition(module: usize, definition: &ast::Def) -> Result<()> {
    let ExprKind::Sql(sql) = &definition.body.kind else {
        return Ok(());
    };
    let Some(ty) = definition.ty.as_ref() else {
        return Ok(());
    };
    let arity = template_arity_from_type(ty, declared_kind(definition));
    check_placeholders(sql, arity)
        .map_err(|error| error.at(Origin::new(module, definition.body.span)))
}

pub(crate) use crate::check::TemplateKind as DeclaredKind;

fn template_arity_from_type(ty: &ast::TypeExpr, kind: DeclaredKind) -> usize {
    let mut arity: usize = 0;
    let mut current = ty;
    while let ast::TypeExpr::Fun(_, result) = current {
        arity += 1;
        current = result;
    }
    if kind == DeclaredKind::Win {
        arity.saturating_sub(1)
    } else {
        arity
    }
}

fn check_placeholders(sql: &str, expression_args: usize) -> Result<()> {
    let bytes = sql.as_bytes();
    let mut seen = vec![false; expression_args + 1];
    let mut index = 0;
    let mut highest = 0;
    let mut in_string = false;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if in_string && bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
            } else {
                in_string = !in_string;
                index += 1;
            }
            continue;
        }
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == index + 1 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: a `$` at byte {start} is not a placeholder"
            )));
        }
        let number = sql[index + 1..end]
            .parse::<usize>()
            .map_err(|_| Error::new("SQL template placeholder is too large"))?;
        let glued = |byte: u8| byte == b'_' || byte.is_ascii_alphanumeric() || byte == b'$';
        if in_string
            || bytes.get(end).copied().is_some_and(glued)
            || (index > 0 && glued(bytes[index - 1]))
        {
            return Err(Error::new(format!(
                "SQL template `{sql}`: `{}` must stand alone (not inside a name or string)",
                &sql[index..end]
            )));
        }
        if number == 0 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: placeholders are 1-based, so `${number}` is not an argument"
            )));
        }
        if number > expression_args {
            return Err(Error::new(format!(
                "template uses placeholders up to ${number} but its signature has {expression_args} expression argument(s)"
            )));
        }
        seen[number] = true;
        highest = highest.max(number);
        index = end;
    }
    if let Some(missing) = (1..=highest).find(|&number| !seen[number]) {
        return Err(Error::new(format!(
            "SQL template `{sql}` is missing placeholder `${missing}`"
        )));
    }
    if highest < expression_args {
        return Err(Error::new(
            format!("template uses placeholders up to ${highest} but its signature has {expression_args} expression argument(s)"),
        ));
    }
    Ok(())
}

fn declared_kind(definition: &ast::Def) -> DeclaredKind {
    crate::check::declared_template_kind(definition)
}
