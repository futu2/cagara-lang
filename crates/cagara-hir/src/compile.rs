//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{elaborate_module, is_not_a_query};
use crate::ir::Rel;
use crate::workspace::{Diag, Workspace};

/// Stable identity for a definition inside a loaded workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefinitionId {
    pub module: usize,
    pub def: usize,
}

/// One root definition that was considered by compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub id: DefinitionId,
    pub name: String,
    pub result: Result<Rel, Diag>,
}

/// The complete result of checking and compiling a workspace's root module.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Compilation {
    pub queries: Vec<CompiledQuery>,
    pub diagnostics: Vec<Diag>,
}

impl Compilation {
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Check and compile every query in the root module.
pub fn compile(ws: &Workspace) -> Compilation {
    let tc = crate::check::check(ws);
    compile_checked(ws, &tc)
}

/// Compile a workspace using a type check the caller already computed.
pub fn compile_checked(ws: &Workspace, tc: &TypeCheck) -> Compilation {
    let mut out = Compilation {
        queries: Vec::new(),
        diagnostics: ws.diags.clone(),
    };
    for error in &tc.errors {
        push_unique(&mut out.diagnostics, error.diag.clone());
    }

    for (index, (name, result)) in elaborate_module(ws, tc, ws.root).into_iter().enumerate() {
        let id = DefinitionId {
            module: ws.root,
            def: index,
        };
        let result = match result {
            Ok(query) => match crate::checked::erase(query) {
                Ok(rel) => match crate::schema::schema_located(&rel) {
                    Ok(_) => Ok(rel),
                    Err((location, message)) => Err(diagnostic_at(
                        ws,
                        location.map(|l| Origin::new(l.module, l.span)),
                        message,
                    )),
                },
                Err(error) => Err(internal_error(ws, &name, &error)),
            },
            Err(error) if is_not_a_query(&error) => {
                if let Some(diag) = tc.error_for(ws.root, index) {
                    Err(diag.clone())
                } else {
                    continue;
                }
            }
            Err(error) => {
                let diag = match error.fault {
                    Fault::Program => diagnostic_at(ws, error.origin, error.message),
                    Fault::Compiler => internal_error(ws, &name, &error),
                    Fault::NotAQuery => unreachable!("handled above"),
                };
                Err(diag)
            }
        };
        if let Err(diag) = &result {
            push_unique(&mut out.diagnostics, diag.clone());
        }
        out.queries.push(CompiledQuery { id, name, result });
    }
    out
}

/// Elaborate root definitions directly into checked queries and erase them.
///
/// Type checking supplies the facts used by source elaboration. A checked
/// query is the sole source of a production `Rel`.
pub fn root_queries_checked(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>)> {
    compile_checked(ws, tc)
        .queries
        .into_iter()
        .map(|query| (query.name, query.result))
        .collect()
}

/// Check and compile every query in the root module.
pub fn root_queries(ws: &Workspace) -> Vec<(String, Result<Rel, Diag>)> {
    compile(ws)
        .queries
        .into_iter()
        .map(|query| (query.name, query.result))
        .collect()
}

fn push_unique(diags: &mut Vec<Diag>, diag: Diag) {
    if !diags.contains(&diag) {
        diags.push(diag);
    }
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
        let compilation = compile_checked(&ws, &tc);
        assert!(compilation.diagnostics.is_empty());
        let tables: Vec<_> = compilation
            .queries
            .into_iter()
            .map(|query| match query.result.unwrap() {
                Rel::At(_, rel) => match *rel {
                    Rel::Table { name, .. } => name,
                    other => panic!("expected table, got {other:?}"),
                },
                other => panic!("expected located table, got {other:?}"),
            })
            .collect();
        assert_eq!(tables, vec!["one", "two"]);

        let compilation = compile(&ws);
        assert_eq!(
            compilation
                .queries
                .iter()
                .map(|query| query.id.def)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
