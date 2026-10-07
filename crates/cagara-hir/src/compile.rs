//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{elaborate_definition, is_not_a_query};
use crate::ir::Rel;
use crate::workspace::{Binding, Diag, LoadedModule, Workspace};
use cagara_syntax::ast::Module;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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

/// The source-based identity of a definition inside a loaded workspace.
///
/// `def` in [`DefinitionId`] is a source-order position, so inserting a
/// definition changes the ids of everything after it. This key is independent
/// of that position: it fingerprints the definition's source and uses an
/// occurrence number only to distinguish same-name definitions. The numeric
/// id remains available for APIs that need to address the AST directly.
/// Keys are scoped to a workspace, not a persistent cache format. Hashes may
/// collide; the incremental shell compares exact facts before reusing a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefinitionKey {
    pub module: usize,
    pub source_hash: u64,
    pub occurrence: usize,
}

/// The numeric source location of a definition in a loaded module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefinitionId {
    pub module: usize,
    pub def: usize,
}

/// One root definition that was considered by compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub id: DefinitionId,
    pub key: DefinitionKey,
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
    let previous = ws.take_previous_compilation();
    let cached = crate::incremental::compile_current(ws, previous);
    let compilation = cached.compilation.clone();
    ws.set_compilation_cache(cached);
    compilation
}

/// Return diagnostics while avoiding a full clone of cached query trees.
pub fn compile_diagnostics(ws: &Workspace) -> Vec<Diag> {
    if let Some(diagnostics) = ws.compilation_diagnostics() {
        return diagnostics;
    }
    let previous = ws.take_previous_compilation();
    let cached = crate::incremental::compile_current(ws, previous);
    let diagnostics = cached.compilation.diagnostics.clone();
    ws.set_compilation_cache(cached);
    diagnostics
}

/// Compile a workspace using a type check the caller already computed.
pub fn compile_checked(ws: &Workspace, tc: &TypeCheck) -> Compilation {
    compile_input(CompilerInput::new(ws, tc))
}

/// Compile an immutable view over cached workspace data.
pub fn compile_input(input: CompilerInput<'_>) -> Compilation {
    compile_reusing(input, HashMap::new())
}

/// The shell supplies only relations whose complete inputs are unchanged.
pub(crate) fn compile_reusing(
    input: CompilerInput<'_>,
    mut reuse: HashMap<DefinitionId, Result<Rel, Diag>>,
) -> Compilation {
    let tc = input.type_check();
    let root = input.root();
    let keys = definition_keys(root);
    let mut out = Compilation {
        queries: Vec::new(),
        diagnostics: input.diagnostics().to_vec(),
    };
    for error in &tc.errors {
        push_unique(&mut out.diagnostics, error.diag.clone());
    }

    for (index, definition) in root.source().defs.iter().enumerate() {
        let name = definition.name.clone();
        let key = keys[index];
        let id = DefinitionId {
            module: root.index(),
            def: index,
        };
        let result = reuse.remove(&id).or_else(|| {
            match elaborate_definition(input, root.index(), index, definition) {
                Ok(query) => Some(match crate::checked::erase(query) {
                    Ok(rel) => match crate::schema::schema_located(&rel) {
                        Ok(_) => Ok(rel),
                        Err((location, message)) => Err(input
                            .diagnostic(location.map(|l| Origin::new(l.module, l.span)), message)),
                    },
                    Err(error) => Err(internal_error(input, &name, &error)),
                }),
                Err(error) if is_not_a_query(&error) => {
                    tc.error_for(root.index(), index).cloned().map(Err)
                }
                Err(error) => Some(Err(match error.fault {
                    Fault::Program => input.diagnostic(error.origin, error.message),
                    Fault::Compiler => internal_error(input, &name, &error),
                    Fault::NotAQuery => unreachable!("handled above"),
                })),
            }
        });
        let Some(result) = result else {
            continue;
        };
        if let Err(diag) = &result {
            push_unique(&mut out.diagnostics, diag.clone());
        }
        out.queries.push(CompiledQuery {
            id,
            key,
            name,
            result,
        });
    }
    out
}

fn source_hash(source: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

/// Build source-based identities before elaboration drops non-query
/// definitions from the result. Occurrences are counted by name, so editing a
/// definition's body does not renumber a later overload with the same name.
fn definition_keys(module: ModuleSnapshot<'_>) -> Vec<DefinitionKey> {
    let mut occurrences = HashMap::<&str, usize>::new();
    module
        .source()
        .defs
        .iter()
        .map(|definition| {
            let occurrence = occurrences.entry(definition.name.as_str()).or_insert(0);
            let source = module
                .text()
                .get(definition.span.start as usize..definition.span.end as usize)
                .unwrap_or_default();
            let key = DefinitionKey {
                module: module.index(),
                source_hash: source_hash(source),
                occurrence: *occurrence,
            };
            *occurrence += 1;
            key
        })
        .collect()
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

fn internal_error(input: CompilerInput<'_>, name: &str, error: &Error) -> Diag {
    let detail = error.message.clone();
    let message = format!("internal error in `{name}`: {detail}");
    input.diagnostic(error.origin, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matches_fresh(ws: &Workspace) -> Compilation {
        let cached = compile(ws);
        let fresh = compile_checked(ws, &crate::check::check(ws));
        assert_eq!(cached, fresh);
        cached
    }

    #[test]
    fn cached_compilation_matches_fresh_across_edits_errors_and_overloads() {
        let table = "users : query { a = int } = table \"s\" \"one\"\n";
        let query = "q = users & select { v = .a + 1 }\n";
        let initial = format!("{table}{query}");
        let mut ws = Workspace::from_source(&initial);
        assert!(assert_matches_fresh(&ws).is_ok());
        for source in [
            initial.replace("one", "two"),
            format!("{table}alias = users\nq = alias & select {{ v = .a + 1 }}\n"),
            initial.replace("a = int", "a = float"),
            initial.replace("a = int", "a = bool"),
            initial.replace(".a + 1", ".missing + 1"),
            format!("# héllo\n{initial}"),
            "q = (\n".into(),
            initial.clone(),
            format!("{table}{table}{query}"),
            format!("{table}q = 1\n"),
            String::new(),
            initial,
        ] {
            assert!(ws.set_source(ws.root, source));
            assert_matches_fresh(&ws);
        }
    }

    #[test]
    fn imported_dependency_edits_and_locations_match_fresh_compilation() {
        let root_path = std::path::PathBuf::from("/tmp/cagara-facts/main.cagara");
        let lib_path = std::path::PathBuf::from("/tmp/cagara-facts/lib.cagara");
        let root = "import \"lib.cagara\" as lib\nq = lib.value\n";
        let lib = "value : query { a = int } = table \"s\" \"one\"\n";
        let buffers = HashMap::from([(lib_path.clone(), lib.to_string())]);
        let mut ws = Workspace::open_with_buffers(&root_path, root.into(), &buffers);
        assert!(assert_matches_fresh(&ws).is_ok());
        let module = ws.module_for_path(&lib_path).unwrap();
        for source in [
            lib.replace("one", "two"),
            format!("# moved\n{lib}"),
            "value = false\n".into(),
            lib.into(),
        ] {
            assert!(ws.set_source(module, source));
            assert_matches_fresh(&ws);
        }
        // Several accepted edits before the next request still compare with
        // the last completed compilation.
        assert!(ws.set_source(module, lib.replace("one", "two")));
        assert!(ws.set_source(module, lib.replace("one", "six")));
        assert_matches_fresh(&ws);
    }

    #[test]
    fn transitive_users_change_while_an_independent_query_is_reused() {
        let src = "base : query { a = int } = table \"s\" \"one\"\n\
                   alias = base\nq = alias\n\
                   alone : query { a = int } = table \"s\" \"own\"\n";
        let mut ws = Workspace::from_source(src);
        assert!(compile(&ws).is_ok());
        let before = crate::elaborate::elaborated_defs().len();
        assert!(ws.set_source(ws.root, src.replace("\"one\"", "\"two\"")));
        let edited = compile(&ws);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0), (ws.root, 1), (ws.root, 2)]
        );
        assert_eq!(edited, compile_checked(&ws, &crate::check::check(&ws)));
    }

    #[test]
    fn dependency_walk_handles_long_chains_and_cycles_without_recursing() {
        let mut source = "d0 = 1\n".to_string();
        for i in 1..10_000 {
            source.push_str(&format!("d{i} = d{}\n", i - 1));
        }
        let ws = Workspace::from_source(&source);
        assert!(compile(&ws).is_ok());
        let mut ws = Workspace::from_source("a = b\nb = a\n");
        assert_matches_fresh(&ws);
        assert!(ws.set_source(ws.root, "a = b\nb = 1\n".into()));
        assert_matches_fresh(&ws);
    }

    #[test]
    fn cached_relations_keep_current_source_locations() {
        let src = "q : query { a = int } = table \"public\" \"items\"\n";
        let mut ws = Workspace::from_source(src);
        assert!(compile(&ws).is_ok());
        assert!(ws.set_source(ws.root, format!("# moved\n{src}")));
        let cached = compile(&ws);
        let fresh = compile_checked(&ws, &crate::check::check(&ws));
        assert_eq!(
            cached, fresh,
            "cache reuse must preserve every Rel::At span"
        );
    }

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
        assert_ne!(
            compilation.queries[0].key, compilation.queries[1].key,
            "duplicate definitions need distinct cache identities"
        );
    }

    #[test]
    fn definition_keys_survive_unrelated_insertions() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        let before = compile(&ws);
        let keys_before: Vec<_> = before.queries.iter().map(|query| query.key).collect();

        assert!(ws.set_source(
            ws.root,
            "inserted : query { a = int } = table \"public\" \"inserted\"\n\
             first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);
        let by_name: HashMap<_, _> = after
            .queries
            .iter()
            .map(|query| (query.name.as_str(), query.key))
            .collect();

        assert_eq!(by_name["first"], keys_before[0]);
        assert_eq!(by_name["second"], keys_before[1]);
    }

    #[test]
    fn inserting_a_definition_refreshes_moved_query_locations() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        compile(&ws);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "inserted : query { a = int } = table \"public\" \"inserted\"\n\
             first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);

        assert!(after.diagnostics.is_empty(), "{:?}", after.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0), (ws.root, 1), (ws.root, 2)],
            "moved queries need new source locations"
        );
        assert_eq!(after, compile_checked(&ws, &crate::check::check(&ws)));
    }

    #[test]
    fn changing_a_dependency_reelaborates_its_users() {
        let mut ws = Workspace::from_source(
            "base : query { a = int } = table \"public\" \"base\"\n\
             q = base\n",
        );
        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "base : query { a = int } = table \"public\" \"changed\"\n\
             q = base\n"
                .into()
        ));
        let edited = compile(&ws);

        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0), (ws.root, 1)],
            "a dependent definition must not reuse a stale expanded query"
        );
    }

    #[test]
    #[ignore = "large-definition benchmark; set CAGARA_BENCH_DEFINITIONS to scale it"]
    fn large_definition_cache_benchmark() {
        let count = std::env::var("CAGARA_BENCH_DEFINITIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100_000);
        let mut source = String::with_capacity(count.saturating_mul(64));
        for i in 0..count {
            source.push_str(&format!(
                "d{i} : query {{ a = int }} = table \"public\" \"t{i}\"\n"
            ));
        }
        let mut ws = Workspace::from_source(&source);
        assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
        let started = std::time::Instant::now();
        let type_check = crate::check::check(&ws);
        let check_elapsed = started.elapsed();
        assert!(type_check.errors.is_empty(), "{:?}", type_check.errors);
        println!("{count} definitions: type check {check_elapsed:?}");

        let started = std::time::Instant::now();
        let first = compile(&ws);
        let first_elapsed = started.elapsed();
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        println!("{count} definitions: first compilation {first_elapsed:?}");

        let edit_at = source.find("t0\"").expect("first table name") + 1;
        let mut edit = source.clone();
        edit.replace_range(edit_at..edit_at + 1, "x");
        let before_edit = crate::elaborate::elaborated_defs().len();
        assert!(ws.set_source(ws.root, edit));
        let started = std::time::Instant::now();
        let type_check = crate::check::check(&ws);
        let check_elapsed = started.elapsed();
        assert!(type_check.errors.is_empty(), "{:?}", type_check.errors);
        println!("{count} definitions: edited type check {check_elapsed:?}");
        let started = std::time::Instant::now();
        let second = compile(&ws);
        let second_elapsed = started.elapsed();
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborated_defs().len() - before_edit,
            1,
            "one-definition edits should elaborate one query"
        );
        println!("{count} definitions: cached one-edit compilation {second_elapsed:?}");
    }

    #[test]
    fn editing_one_definition_changes_only_its_key() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        let before = compile(&ws);
        let first_before = before.queries[0].key;
        let second_before = before.queries[1].key;

        assert!(ws.set_source(
            ws.root,
            "first : query { a = int } = table \"public\" \"changed\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);

        assert_ne!(after.queries[0].key, first_before);
        assert_eq!(after.queries[1].key, second_before);
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
    fn editing_one_definition_reuses_unchanged_root_definitions() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "first : query { a = int } = table \"public\" \"other\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let edited = compile(&ws);
        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0)],
            "an unchanged definition should reuse its erased query"
        );
    }
}
