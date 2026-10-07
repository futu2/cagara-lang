//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{elaborate_module, is_not_a_query};
use crate::ir::Rel;
use crate::workspace::{Binding, Diag, LoadedModule, Workspace};
use cagara_syntax::ast::Module;
use std::collections::HashMap;
use std::path::Path;

/// Immutable compiler input borrowed from an existing workspace.
///
/// The workspace remains the owner of Salsa storage, overlays, and cache
/// identity. This view only bundles the cached source graph and its type-check
/// result for the source-elaboration and erasure boundary.
#[derive(Clone, Copy)]
pub struct CompilerInput<'a> {
    workspace: &'a Workspace,
    type_check: &'a TypeCheck,
}

impl<'a> CompilerInput<'a> {
    pub fn new(workspace: &'a Workspace, type_check: &'a TypeCheck) -> Self {
        Self {
            workspace,
            type_check,
        }
    }

    pub fn type_check(&self) -> &'a TypeCheck {
        self.type_check
    }

    pub fn diagnostics(&self) -> &'a [Diag] {
        &self.workspace.diags
    }

    pub fn diagnostic(&self, origin: Option<Origin>, message: impl Into<String>) -> Diag {
        match origin {
            Some(origin) => self
                .workspace
                .diag_span(origin.module, origin.span, message),
            None => self.workspace.diag(self.workspace.root, 0, message),
        }
    }

    pub fn root(&self) -> ModuleSnapshot<'a> {
        self.module(self.workspace.root)
    }

    pub fn module(&self, module: usize) -> ModuleSnapshot<'a> {
        ModuleSnapshot {
            workspace: self.workspace,
            module,
        }
    }
}

/// Read-only view of one loaded module in a [`CompilerInput`].
#[derive(Clone, Copy)]
pub struct ModuleSnapshot<'a> {
    workspace: &'a Workspace,
    module: usize,
}

impl<'a> ModuleSnapshot<'a> {
    pub fn index(&self) -> usize {
        self.module
    }

    pub fn path(&self) -> &'a Path {
        &self.loaded().path
    }

    pub fn text(&self) -> &'a str {
        &self.loaded().text
    }

    pub fn source(&self) -> &'a Module {
        &self.loaded().module
    }

    pub fn def(&self, def: usize) -> &'a cagara_syntax::ast::Def {
        &self.source().defs[def]
    }

    pub fn scope(&self) -> &'a HashMap<String, Binding> {
        &self.loaded().scope
    }

    pub fn own(&self) -> &'a HashMap<String, Binding> {
        &self.loaded().own
    }

    fn loaded(&self) -> &'a LoadedModule {
        &self.workspace.modules[self.module]
    }
}

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
    if let Some(cached) = ws.compilation_cache() {
        return cached;
    }
    let compilation = compile_uncached(ws);
    ws.set_compilation_cache(compilation.clone());
    compilation
}

/// Return diagnostics while avoiding a full clone of cached query trees.
pub fn compile_diagnostics(ws: &Workspace) -> Vec<Diag> {
    if let Some(diagnostics) = ws.compilation_diagnostics() {
        return diagnostics;
    }
    let compilation = compile_uncached(ws);
    let diagnostics = compilation.diagnostics.clone();
    ws.set_compilation_cache(compilation);
    diagnostics
}

fn compile_uncached(ws: &Workspace) -> Compilation {
    let tc = crate::check::check(ws);
    compile_input(CompilerInput::new(ws, &tc))
}

/// Compile a workspace using a type check the caller already computed.
pub fn compile_checked(ws: &Workspace, tc: &TypeCheck) -> Compilation {
    compile_input(CompilerInput::new(ws, tc))
}

/// Compile an immutable view over cached workspace data.
pub fn compile_input(input: CompilerInput<'_>) -> Compilation {
    let tc = input.type_check();
    let root = input.root();
    let mut out = Compilation {
        queries: Vec::new(),
        diagnostics: input.diagnostics().to_vec(),
    };
    for error in &tc.errors {
        push_unique(&mut out.diagnostics, error.diag.clone());
    }

    for (index, (name, result)) in elaborate_module(input, root.index())
        .into_iter()
        .enumerate()
    {
        let id = DefinitionId {
            module: root.index(),
            def: index,
        };
        let result =
            match result {
                Ok(query) => match crate::checked::erase(query) {
                    Ok(rel) => match crate::schema::schema_located(&rel) {
                        Ok(_) => Ok(rel),
                        Err((location, message)) => Err(input
                            .diagnostic(location.map(|l| Origin::new(l.module, l.span)), message)),
                    },
                    Err(error) => Err(internal_error(input, &name, &error)),
                },
                Err(error) if is_not_a_query(&error) => {
                    if let Some(diag) = tc.error_for(root.index(), index) {
                        Err(diag.clone())
                    } else {
                        continue;
                    }
                }
                Err(error) => {
                    let diag = match error.fault {
                        Fault::Program => input.diagnostic(error.origin, error.message),
                        Fault::Compiler => internal_error(input, &name, &error),
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
    compile_input(CompilerInput::new(ws, tc))
        .queries
        .into_iter()
        .map(|query| (query.name, query.result))
        .collect()
}

/// Check and compile every query in the root module.
pub fn root_queries(ws: &Workspace) -> Vec<(String, Result<Rel, Diag>)> {
    compile_input(CompilerInput::new(ws, &crate::check::check(ws)))
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

fn internal_error(input: CompilerInput<'_>, name: &str, error: &Error) -> Diag {
    let detail = error.message.clone();
    let message = format!("internal error in `{name}`: {detail}");
    input.diagnostic(error.origin, message)
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

    #[test]
    fn compiler_input_is_a_read_only_view_of_the_workspace() {
        let ws = Workspace::from_source("q = 1\n");
        let tc = crate::check::check(&ws);
        let input = CompilerInput::new(&ws, &tc);
        let root = input.root();

        assert_eq!(root.index(), ws.root);
        assert_eq!(root.path(), Path::new("<input>"));
        assert_eq!(root.text(), "q = 1\n");
        assert_eq!(root.source().defs[0].name, "q");
        assert!(root.scope().contains_key("q"));

        assert_eq!(compile_input(input), compile_checked(&ws, &tc));
    }

    #[test]
    fn compiler_input_preserves_source_diagnostics() {
        let ws = Workspace::from_source("q = (\n");
        let tc = crate::check::check(&ws);
        let compilation = compile_input(CompilerInput::new(&ws, &tc));

        assert!(!compilation.diagnostics.is_empty());
        assert_eq!(compilation.diagnostics, ws.diags);
    }

    #[test]
    fn diagnostics_reuse_the_cached_compilation() {
        let ws = Workspace::from_source("q = (\n");

        let first = compile_diagnostics(&ws);
        let after_first = crate::elaborate::elaborate_runs();
        let second = compile_diagnostics(&ws);

        assert_eq!(second, first);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first,
            "cached diagnostics should not re-elaborate the source"
        );
    }

    #[test]
    fn compilation_cache_reuses_and_invalidates_elaboration() {
        let mut ws = Workspace::from_source("q : query { a = int } = table \"public\" \"items\"\n");

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let after_first = crate::elaborate::elaborate_runs();

        let second = compile(&ws);
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first,
            "unchanged compilation should reuse the cached result"
        );

        assert!(ws.set_source(
            ws.root,
            "q : query { a = int } = table \"public\" \"updated\"\n".into()
        ));
        let edited = compile(&ws);
        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first + 1,
            "an accepted edit should invalidate the cached compilation"
        );
    }

    #[test]
    fn changing_the_compilation_root_invalidates_the_cache() {
        let root_path = std::path::PathBuf::from("/tmp/cagara-root-cache/root.cagara");
        let lib_path = std::path::PathBuf::from("/tmp/cagara-root-cache/lib.cagara");
        let root_src = "import \"lib.cagara\" as lib\nq = lib.value\n";
        let lib_src = "value : query { a = int } = table \"public\" \"items\"\n";
        let mut buffers = std::collections::HashMap::new();
        buffers.insert(root_path.clone(), root_src.to_string());
        buffers.insert(lib_path.clone(), lib_src.to_string());
        let mut ws = Workspace::open_with_buffers(&root_path, root_src.to_string(), &buffers);

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let after_first = crate::elaborate::elaborate_runs();

        assert!(ws.set_root_path(&lib_path));
        let second = compile(&ws);
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first + 1,
            "changing roots must not reuse the previous root compilation"
        );
    }

    #[test]
    fn editing_one_definition_currently_reelaborates_all_root_definitions() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "first : query { a = int } = table \"public\" \"changed\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let edited = compile(&ws);
        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0), (ws.root, 1)],
            "the current compilation boundary rebuilds every root definition"
        );
    }
}
