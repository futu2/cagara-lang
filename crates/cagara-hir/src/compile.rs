//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{elaborate_module, is_not_a_query};
use crate::ir::Rel;
use crate::workspace::{Diag, Workspace};

/// Elaborate root definitions directly into checked queries and erase them.
///
/// Type checking supplies the facts used by source elaboration. A checked
/// query is the sole source of a production `Rel`.
pub fn root_queries_checked(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>)> {
    let elaborated = elaborate_module(ws, tc, ws.root);
    let mut out = Vec::new();

    for (index, (name, result)) in elaborated.into_iter().enumerate() {
        match result {
            Ok(query) => match crate::checked::erase(query) {
                Ok(rel) => match crate::schema::schema_located(&rel) {
                    Ok(_) => out.push((name, Ok(rel))),
                    Err((location, message)) => out.push((
                        name,
                        Err(diagnostic_at(
                            ws,
                            location.map(|l| Origin::new(l.module, l.span)),
                            message,
                        )),
                    )),
                },
                Err(error) => out.push((name.clone(), Err(internal_error(ws, &name, &error)))),
            },
            Err(error) if is_not_a_query(&error) => {
                if let Some(diag) = tc.error_for(ws.root, index) {
                    out.push((name, Err(diag.clone())));
                }
            }
            Err(error) => {
                let diag = match error.fault {
                    Fault::Program => diagnostic_at(ws, error.origin, error.message),
                    Fault::Compiler => internal_error(ws, &name, &error),
                    Fault::NotAQuery => unreachable!("handled above"),
                };
                out.push((name, Err(diag)));
            }
        }
    }
    out
}

/// Check and compile every query in the root module.
pub fn root_queries(ws: &Workspace) -> Vec<(String, Result<Rel, Diag>)> {
    root_queries_checked(ws, &crate::check::check(ws))
}

fn diagnostic_at(ws: &Workspace, origin: Option<Origin>, message: impl Into<String>) -> Diag {
    match origin {
        Some(origin) => ws.diag_span(origin.module, origin.span, message),
        None => ws.diag(ws.root, 0, message),
    }
}

fn internal_error(ws: &Workspace, name: &str, error: &Error) -> Diag {
    let detail = error.message.clone();
    let message = format!("internal error in `{name}`: {detail}");
    diagnostic_at(ws, error.origin, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_query_definitions_keep_source_identity() {
        let ws = Workspace::from_source(
            "q : query { a = int } = table \"s\" \"one\"\n\
             q : query { a = int } = table \"s\" \"two\"\n",
        );
        let tc = crate::check::check(&ws);
        let queries = root_queries_checked(&ws, &tc);
        let tables: Vec<_> = queries
            .into_iter()
            .map(|(_, result)| match result.unwrap() {
                Rel::At(_, rel) => match *rel {
                    Rel::Table { name, .. } => name,
                    other => panic!("expected table, got {other:?}"),
                },
                other => panic!("expected located table, got {other:?}"),
            })
            .collect();
        assert_eq!(tables, vec!["one", "two"]);
    }
}
