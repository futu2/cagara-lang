//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{is_not_a_query, prepare_query_definition, record_definition};
use crate::ir::Rel;
use crate::workspace::{Binding, Diag, LoadedModule, Workspace};
use cagara_syntax::ast::Module;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Immutable compiler input borrowed from an existing workspace.
///
/// The workspace remains the owner of Salsa storage, overlays, and cache
/// identity. This view only bundles the cached source graph and its type-check
/// result for normalization and erasure.
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
    /// Who is at fault when `result` is an `Err`.
    ///
    /// The diagnostic text is for a human; this is the machine-readable half of
    /// the same verdict, so a caller can tell a *program* error from a
    /// *compiler capability gap* without matching on the message. The checked
    /// layer already distinguishes the two ([`Fault`]) and the distinctive
    /// "internal error in `x`" prefix exists only as a rendering choice, so
    /// classifying by substring would invert the dependency: tests would be
    /// pinned to user-facing wording.
    ///
    /// `None` when the definition compiled, and also for a definition that was
    /// never a query to begin with (`Fault::NotAQuery`): that is *no verdict*
    /// rather than a failure, and it produces no `result` entry at all.
    pub fault: Option<Fault>,
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
    // Dedupe through a set, not `Vec::contains`: a file with many repeated
    // diagnostics would otherwise be quadratic in the number of diagnostics.
    let mut seen: HashSet<Diag> = out.diagnostics.iter().cloned().collect();
    for error in tc.errors() {
        push_unique(&mut out.diagnostics, &mut seen, error.diag.clone());
    }

    for (index, definition) in root.source().defs.iter().enumerate() {
        let name = definition.name.clone();
        let key = keys[index];
        let id = DefinitionId {
            module: root.index(),
            def: index,
        };
        let compiled = match reuse.remove(&id) {
            // A reused relation is a success by construction: only successful,
            // unaffected queries are ever stored for reuse.
            Some(result) => Some((result, None)),
            None => compile_definition(input, tc, root, index, definition, &name),
        };
        let Some((result, fault)) = compiled else {
            continue;
        };
        if let Err(diag) = &result {
            push_unique(&mut out.diagnostics, &mut seen, diag.clone());
        }
        out.queries.push(CompiledQuery {
            id,
            key,
            name,
            result,
            fault,
        });
    }
    out
}

/// Compile one root definition, reporting **who is at fault** alongside the
/// diagnostic.
///
/// `None` means the definition has no verdict: it is not a query, so there is
/// nothing to build and nothing to report. Every other outcome yields a result
/// and, when it is an `Err`, the [`Fault`] that produced it.
fn compile_definition(
    input: CompilerInput<'_>,
    tc: &TypeCheck,
    root: ModuleSnapshot<'_>,
    index: usize,
    definition: &cagara_syntax::ast::Def,
    name: &str,
) -> Option<(Result<Rel, Diag>, Option<Fault>)> {
    record_definition(root.index(), index);
    match prepare_query_definition(input, root.index(), index, definition) {
        Ok(()) => {}
        // Not a failure at all: a scalar definition, a stage, … has no query to
        // build. The checker's own diagnostic (if it produced one) is the only
        // thing worth reporting here, because elaboration has no verdict.
        Err(error) if is_not_a_query(&error) => {
            let diag = tc.error_for(root.index(), index).cloned()?;
            return Some((Err(diag), Some(Fault::Program)));
        }
        Err(error) => {
            let fault = error.fault;
            let diag = match fault {
                Fault::Program => input.diagnostic(error.origin, error.message),
                Fault::Compiler => internal_error(input, name, &error),
                Fault::NotAQuery => unreachable!("handled above"),
            };
            return Some((Err(diag), Some(fault)));
        }
    }
    // Normalization, erasure, and schema validation follow the query-shape
    // check. A capability gap here is a *compiler* fault: the checker already
    // accepted the definition, so this phase could not build it. Reporting that
    // distinction is the whole point of carrying `Fault`.
    Some(
        match crate::normalize::route::lower(input, root.index(), index, &definition.body) {
            Ok(rel) => match crate::schema::schema_located(&rel) {
                Ok(_) => (Ok(rel), None),
                Err((location, message)) => (
                    Err(input.diagnostic(location.map(|l| Origin::new(l.module, l.span)), message)),
                    Some(Fault::Compiler),
                ),
            },
            Err(error) => {
                let fault = error.fault;
                let diag = match fault {
                    Fault::Program => input.diagnostic(error.origin, error.message),
                    Fault::Compiler => internal_error(input, name, &error),
                    Fault::NotAQuery => unreachable!("the query shape was checked above"),
                };
                (Err(diag), Some(fault))
            }
        },
    )
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

/// Compile root definitions using a type check the caller already computed.
///
/// The checker supplies the facts consumed by the production normalizer; the
/// checked-tree elaboration API remains available for compatibility.
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

/// One root definition's verdict, for a caller that needs to tell a *program*
/// error from a *compiler capability gap*.
///
/// [`root_queries_checked`] collapses both failures to `Err(Diag)`, which is
/// what a renderer wants and exactly what a totality check must not do: a
/// checker-accepted definition a later phase could not build is then
/// indistinguishable from one that was simply malformed. A definition that is
/// not a query at all (`Fault::NotAQuery`) never appears here — it has no
/// verdict, which is different from having a failing one.
pub fn root_verdicts(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>, Fault)> {
    compile_input(CompilerInput::new(ws, tc))
        .queries
        .into_iter()
        .filter_map(|query| {
            let fault = query.fault?;
            Some((query.name, query.result, fault))
        })
        .collect()
}

fn push_unique(diags: &mut Vec<Diag>, seen: &mut HashSet<Diag>, diag: Diag) {
    if seen.insert(diag.clone()) {
        diags.push(diag);
    }
}

fn internal_error(input: CompilerInput<'_>, name: &str, error: &Error) -> Diag {
    let detail = error.message.clone();
    let message = format!("internal error in `{name}`: {detail}");
    input.diagnostic(error.origin, message)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod benchmarks;
