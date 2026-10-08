//! Module loading: the embedded prelude, the root file, and its imports.
//! Parsing goes through the salsa `parse_module` query.

use crate::compile::Compilation;
use crate::db::{Database, ModuleInput, SourceFile};
use crate::incremental::CachedCompilation;
use crate::lower::{parse_module, ParsedModule};
use crate::primitive::Prim;
use crate::resolve::{module_own, module_scope};
use cagara_syntax::ast::{Import, Module, Span};
use salsa::Setter;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const PRELUDE_SRC: &str = include_str!("../../../prelude.cagara");
pub const PRELUDE_PATH: &str = "<prelude>";

#[derive(Debug, Clone, PartialEq)]
pub enum Binding {
    /// (module index, definition index)
    Def(usize, usize),
    /// A name defined more than once with signatures; the type checker picks
    /// one candidate per use. (module index, definition indices)
    Overloads(usize, Vec<usize>),
    Module(usize),
    /// `__` primitives; visible only inside the prelude.
    Prim(Prim),
}

pub struct LoadedModule {
    pub path: PathBuf,
    pub text: String,
    pub module: Module,
    pub scope: Arc<HashMap<String, Binding>>,
    /// This module's own definitions (what `import` and `alias.name` see).
    pub own: Arc<HashMap<String, Binding>>,
    /// Byte offsets where each line of `text` starts. Built once per module so
    /// rendering a diagnostic is O(log lines) instead of rescanning the whole
    /// prefix, which made a file with many diagnostics quadratic.
    line_starts: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diag {
    pub path: String,
    pub line: usize,
    pub col: usize,
    pub message: String,
    /// The source line, for the excerpt (empty if unknown).
    pub source: String,
    /// Characters to underline, starting at `col`.
    pub width: usize,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: error: {}",
            self.path, self.line, self.col, self.message
        )?;
        if !self.source.is_empty() {
            let n = self.line.to_string();
            let pad = self
                .source
                .get(..self.col.saturating_sub(1))
                .map_or(0, |s| s.chars().count());
            let gutter = " ".repeat(n.len());
            write!(
                f,
                "\n {n} | {}\n {gutter} | {}{}",
                self.source,
                " ".repeat(pad),
                "^".repeat(self.width.max(1))
            )?;
        }
        Ok(())
    }
}

pub struct Workspace {
    pub db: Database,
    pub files: HashMap<PathBuf, SourceFile>,
    pub modules: Vec<LoadedModule>,
    pub root: usize,
    pub diags: Vec<Diag>,
    /// Salsa input of each module (same indices as `modules`).
    pub inputs: Vec<ModuleInput>,
    /// Import errors, and each module's own diagnostics (syntax, overloads);
    /// `diags` is their concatenation, rebuilt after an edit.
    import_diags: Vec<Diag>,
    file_diags: Vec<Vec<Diag>>,
    by_path: HashMap<PathBuf, usize>,
    stack: Vec<PathBuf>,
    /// Unsaved contents supplied by an editor. These take precedence over
    /// files on disk while imports are loaded.
    overlays: HashMap<PathBuf, String>,
    /// Immutable compilation result for the current root and source graph.
    /// Invalidated by the mutation methods below.
    compile_cache: RefCell<Option<CachedCompilation>>,
    /// The last compilation remains available after an accepted edit so the
    /// compiler can reuse definitions whose source and checked dependencies
    /// are unchanged.
    previous_compile_cache: RefCell<Option<CachedCompilation>>,
}

impl Workspace {
    fn empty() -> Self {
        let mut ws = Workspace {
            db: Database::default(),
            files: HashMap::new(),
            modules: Vec::new(),
            root: 0,
            diags: Vec::new(),
            inputs: Vec::new(),
            import_diags: Vec::new(),
            file_diags: Vec::new(),
            by_path: HashMap::new(),
            stack: Vec::new(),
            overlays: HashMap::new(),
            compile_cache: RefCell::new(None),
            previous_compile_cache: RefCell::new(None),
        };
        ws.add(PathBuf::from(PRELUDE_PATH), PRELUDE_SRC.to_string());
        ws
    }

    /// Workspace whose root module is an in-memory source (imports resolve
    /// relative to the current directory).
    pub fn from_source(src: &str) -> Self {
        let mut ws = Self::empty();
        ws.root = ws.add(PathBuf::from("<input>"), src.to_string());
        ws.rebuild_diags();
        ws
    }

    pub fn open(path: &Path) -> Self {
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        match std::fs::read_to_string(&p) {
            Ok(text) => Self::open_with(&p, text),
            Err(e) => {
                let mut ws = Self::empty();
                ws.import_diags.push(Diag {
                    path: p.display().to_string(),
                    line: 1,
                    col: 1,
                    message: format!("cannot read file: {e}"),
                    source: String::new(),
                    width: 0,
                });
                ws.rebuild_diags();
                ws
            }
        }
    }

    /// Workspace whose root is the file `path` with the given (possibly
    /// unsaved) contents; imports resolve relative to `path` and are read
    /// from disk. Used by the language server.
    pub fn open_with(path: &Path, text: String) -> Self {
        let mut overlays = HashMap::new();
        overlays.insert(normalize(path), text.clone());
        Self::open_with_buffers(path, text, &overlays)
    }

    /// Workspace rooted at `path`, loading every open buffer as an overlay.
    /// Imports use an overlay when one exists and otherwise read from disk.
    /// Buffers not reachable from the root are also loaded so another open
    /// document can become the root without rebuilding the server's state.
    pub fn open_with_buffers(
        path: &Path,
        text: String,
        buffers: &HashMap<PathBuf, String>,
    ) -> Self {
        let root = normalize(path);
        let mut ws = Self::empty();
        ws.overlays = buffers
            .iter()
            .map(|(p, text)| (normalize(p), text.clone()))
            .collect();
        ws.overlays.entry(root.clone()).or_insert(text.clone());
        let root_text = ws.overlays.get(&root).cloned().unwrap_or(text);
        ws.root = ws.add(root.clone(), root_text);
        let remaining: Vec<(PathBuf, String)> = ws
            .overlays
            .iter()
            .filter(|(p, _)| !ws.by_path.contains_key(*p))
            .map(|(p, text)| (p.clone(), text.clone()))
            .collect();
        for (p, text) in remaining {
            ws.add(p, text);
        }
        ws.rebuild_diags();
        ws
    }

    /// Select a loaded module as the root used by analysis and evaluation.
    pub fn set_root_path(&mut self, path: &Path) -> bool {
        let path = normalize(path);
        let Some(&root) = self.by_path.get(&path) else {
            return false;
        };
        if self.root != root {
            self.invalidate_compile_cache();
        }
        self.root = root;
        true
    }

    /// Return the loaded module for a path, if it is present in this graph.
    pub fn module_for_path(&self, path: &Path) -> Option<usize> {
        self.by_path.get(&normalize(path)).copied()
    }

    /// Rebuild this graph from the loaded module texts. Used for speculative
    /// editor queries so probing a field never mutates the live workspace.
    pub fn snapshot(&self) -> Self {
        let root = self.modules[self.root].path.clone();
        let buffers: HashMap<_, _> = self
            .modules
            .iter()
            .filter(|m| m.path != Path::new(PRELUDE_PATH))
            .map(|m| (m.path.clone(), m.text.clone()))
            .collect();
        Self::open_with_buffers(&root, self.modules[self.root].text.clone(), &buffers)
    }

    fn add(&mut self, path: PathBuf, text: String) -> usize {
        self.stack.push(path.clone());
        let file = SourceFile::new(&self.db, text.clone());
        self.files.insert(path.clone(), file);
        let parsed = parse_module(&self.db, file).clone();

        let prelude = if self.modules.is_empty() {
            None
        } else {
            Some(self.inputs[0])
        };
        let mut imports = Vec::new();
        for imp in &parsed.module.imports {
            let Some(target) = self.import(&path, &text, imp) else {
                continue;
            };
            imports.push((imp.alias.clone(), self.inputs[target]));
        }

        self.stack.pop();
        let m = self.modules.len();
        self.by_path.insert(path.clone(), m);
        let input = ModuleInput::new(&self.db, m, path.clone(), file, prelude, imports);
        let own = module_own(&self.db, input).clone();
        let scope = module_scope(&self.db, input).clone();
        self.file_diags
            .push(file_diags(&path, &text, &parsed, &own));
        let starts = line_starts(&text);
        self.inputs.push(input);
        self.modules.push(LoadedModule {
            path,
            text,
            module: parsed.module,
            scope,
            own,
            line_starts: starts,
        });
        m
    }

    /// Replace the text of loaded module `m`. Parsing, name resolution, and
    /// type checking are salsa queries, so only what depends on the file is
    /// recomputed. Returns `false` if its imports changed; the workspace
    /// must then be reloaded, since loading files is outside salsa.
    ///
    /// This is the only way to edit a loaded file. Salsa's `SourceFile` and
    /// `modules[m].text` are updated together here and nowhere else, so a
    /// span from a query always indexes the text the diagnostics are
    /// rendered against (see `db.rs`).
    pub fn set_source(&mut self, m: usize, text: String) -> bool {
        // Parse the candidate text up front, so a rejected edit (one that
        // changes the imports) leaves both texts as they were.
        let parsed = parse_text(&text);
        if imports(&parsed.module) != imports(&self.modules[m].module) {
            return false;
        }
        self.invalidate_compile_cache();
        let file = *self.inputs[m].file(&self.db);
        // Set the file's text without going through a helper: this changes
        // what the queries parse but nothing else, so `Workspace` must update
        // `modules[m].text` in the same breath (below) or the two phases
        // would analyze different programs.
        file.set_text(&mut self.db).to(text.clone());
        let path = self.modules[m].path.clone();
        // Exports of `m` feed the scopes of its importers: refresh all
        // modules (the queries recompute only what changed, and immutable
        // maps are shared without copying unrelated catalogs).
        for k in 0..self.modules.len() {
            let input = self.inputs[k];
            self.modules[k].own = module_own(&self.db, input).clone();
            self.modules[k].scope = module_scope(&self.db, input).clone();
        }
        self.file_diags[m] = file_diags(&path, &text, &parsed, &self.modules[m].own);
        let starts = line_starts(&text);
        let md = &mut self.modules[m];
        md.text = text;
        md.module = parsed.module;
        md.line_starts = starts;
        self.rebuild_diags();
        true
    }

    pub(crate) fn compilation_cache(&self) -> Option<Compilation> {
        self.compile_cache
            .borrow()
            .as_ref()
            .map(|cached| cached.compilation.clone())
    }

    pub(crate) fn compilation_diagnostics(&self) -> Option<Vec<Diag>> {
        self.compile_cache
            .borrow()
            .as_ref()
            .map(|cached| cached.compilation.diagnostics.clone())
    }

    pub(crate) fn set_compilation_cache(&self, compilation: CachedCompilation) {
        *self.compile_cache.borrow_mut() = Some(compilation);
    }

    pub(crate) fn take_previous_compilation(&self) -> Option<CachedCompilation> {
        self.previous_compile_cache.borrow_mut().take()
    }

    fn invalidate_compile_cache(&mut self) {
        if let Some(previous) = self.compile_cache.get_mut().take() {
            *self.previous_compile_cache.get_mut() = Some(previous);
        }
    }

    fn rebuild_diags(&mut self) {
        self.diags = self
            .import_diags
            .iter()
            .chain(self.file_diags.iter().flatten())
            .cloned()
            .collect();
    }

    fn import(&mut self, from: &Path, text: &str, imp: &Import) -> Option<usize> {
        let base = from.parent().unwrap_or(Path::new("."));
        let raw = base.join(&imp.path);
        let target = normalize(&raw);
        if let Some(&i) = self.by_path.get(&target) {
            return Some(i);
        }
        let offset = imp.span.start as usize;
        if self.stack.contains(&target) {
            let msg = format!("import cycle: `{}` is already being loaded", imp.path);
            self.import_diags.push(make_diag(from, text, offset, msg));
            return None;
        }
        match self
            .overlays
            .get(&target)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| std::fs::read_to_string(&target))
        {
            Ok(t) => Some(self.add(target, t)),
            Err(e) => {
                let msg = format!("cannot import `{}`: {e}", imp.path);
                self.import_diags.push(make_diag(from, text, offset, msg));
                None
            }
        }
    }

    pub fn diag(&self, m: usize, offset: usize, message: impl Into<String>) -> Diag {
        let md = &self.modules[m];
        make_diag_indexed(
            &md.path,
            &md.text,
            &md.line_starts,
            offset,
            offset,
            message.into(),
        )
    }

    /// A diagnostic underlining `span` in module `m`.
    pub fn diag_span(&self, m: usize, span: Span, message: impl Into<String>) -> Diag {
        let md = &self.modules[m];
        make_diag_indexed(
            &md.path,
            &md.text,
            &md.line_starts,
            span.start as usize,
            span.end as usize,
            message.into(),
        )
    }
}

fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A module's import list, as `(path, alias)` in source order. Two texts
/// agree on their imports when these are equal.
fn imports(md: &Module) -> Vec<(String, Option<String>)> {
    md.imports
        .iter()
        .map(|i| (i.path.clone(), i.alias.clone()))
        .collect()
}

/// Parse a file that is not (yet) in the db.
fn parse_text(text: &str) -> ParsedModule {
    let (module, errors) = cagara_syntax::ast::lower_source(text);
    ParsedModule { module, errors }
}

/// Syntax errors of a file, and overloads missing a signature.
fn file_diags(
    path: &Path,
    text: &str,
    parsed: &ParsedModule,
    own: &HashMap<String, Binding>,
) -> Vec<Diag> {
    // One line index for the whole file: a syntax-error-heavy file would
    // otherwise rescan the text once per diagnostic.
    let starts = line_starts(text);
    let mut out: Vec<Diag> = parsed
        .errors
        .iter()
        .map(|e| {
            make_diag_indexed(
                path,
                text,
                &starts,
                e.offset,
                e.offset,
                format!("syntax error: {}", e.message),
            )
        })
        .collect();
    // An operator's fixity is a property of the language, not of one module:
    // the table is built once, from the prelude. A declaration anywhere else
    // would silently do nothing (the file it appears in is already parsed by
    // the time it could be read), so say so instead.
    if path != Path::new(PRELUDE_PATH) {
        for d in &parsed.module.operators {
            out.push(make_diag_indexed(
                path,
                text,
                &starts,
                d.span.start as usize,
                d.span.start as usize,
                format!(
                    "operator `{}` must be declared in `prelude.cagara`, where \
                     `_{}_` would be the name it desugars to",
                    d.spelling, d.spelling
                ),
            ));
        }
    }
    let mut missing: Vec<(usize, &str)> = Vec::new();
    for (name, b) in own {
        if let Binding::Overloads(_, is) = b {
            missing.extend(
                is.iter()
                    .filter(|&&i| parsed.module.defs[i].ty.is_none())
                    .map(|&i| (i, name.as_str())),
            );
        }
    }
    missing.sort();
    for (i, name) in missing {
        let msg = format!("`{name}` is defined more than once, so each definition needs a type signature (overloading)");
        out.push(make_diag_indexed(
            path,
            text,
            &starts,
            parsed.module.defs[i].span.start as usize,
            parsed.module.defs[i].span.start as usize,
            msg,
        ));
    }
    out
}

fn make_diag(path: &Path, text: &str, offset: usize, message: String) -> Diag {
    make_diag_range(path, text, offset, offset, message)
}

/// Byte offsets where each line of `text` starts, always including `0`.
///
/// `pub(crate)` because the memoized module check renders a module's
/// diagnostics once, when its text changes, instead of on every `check()`.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut out = Vec::with_capacity(text.len() / 32 + 1);
    out.push(0);
    out.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    out
}

/// 1-based line number of `offset`, given the line starts.
fn line_at(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|&s| s <= offset).max(1)
}

/// Build a diagnostic from a precomputed line index. Prefer this over
/// [`make_diag_range`] whenever several diagnostics share one text.
pub(crate) fn make_diag_indexed(
    path: &Path,
    text: &str,
    starts: &[usize],
    start: usize,
    end: usize,
    message: String,
) -> Diag {
    // An offset from a parse of a *different* text can land inside a
    // multi-byte character of this one. Snap to a boundary rather than
    // panicking: a diagnostic is not worth crashing over.
    let mut start = start.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = end.clamp(start, text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let line = line_at(starts, start);
    let line_start = starts[line - 1];
    let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let col = start - line_start + 1;
    // Underline up to the end of the span or of the line, whichever is first.
    let width = text[start..end.min(line_end)].chars().count();
    Diag {
        path: path.display().to_string(),
        line,
        col,
        message,
        source: text[line_start..line_end].trim_end().to_string(),
        width,
    }
}

/// [`make_diag_indexed`] for a caller with no line index: builds one per call,
/// so it is linear in the text length. Use the indexed form for repeated calls.
fn make_diag_range(path: &Path, text: &str, start: usize, end: usize, message: String) -> Diag {
    make_diag_indexed(path, text, &line_starts(text), start, end, message)
}

#[cfg(test)]
mod tests {
    use super::Workspace;
    use crate::db::SourceFile;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn open_buffers_override_imports_and_share_a_graph() {
        let root = PathBuf::from("/tmp/cagara-shared/root.cagara");
        let imported = PathBuf::from("/tmp/cagara-shared/lib.cagara");
        let mut buffers: HashMap<PathBuf, String> = HashMap::new();
        buffers.insert(
            root.clone(),
            "import \"lib.cagara\" as lib\nq = lib.value\n".into(),
        );
        buffers.insert(imported.clone(), "value = 42\n".into());
        let ws = Workspace::open_with_buffers(&root, buffers[&root].clone(), &buffers);
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        assert!(ws
            .module_for_path(Path::new("/tmp/cagara-shared/lib.cagara"))
            .is_some());
        assert_eq!(ws.modules[ws.root].module.defs[0].name, "q");
    }

    #[test]
    fn edits_share_unchanged_scopes_and_preserve_previous_exports() {
        use std::sync::Arc;

        let root = PathBuf::from("/tmp/cagara-scopes/root.cagara");
        let lib = root.with_file_name("lib.cagara");
        let source = "import \"lib.cagara\"\nq = value\n";
        let buffers = HashMap::from([(lib.clone(), "value = 42\n".into())]);
        let mut ws = Workspace::open_with_buffers(&root, source.into(), &buffers);
        let imported = ws.module_for_path(&lib).unwrap();
        let exports = Arc::clone(&ws.modules[imported].own);
        let scope = Arc::clone(&ws.modules[imported].scope);

        assert!(ws.set_source(ws.root, source.replace("q = value", "q = value + 1")));
        assert!(Arc::ptr_eq(&exports, &ws.modules[imported].own));
        assert!(Arc::ptr_eq(&scope, &ws.modules[imported].scope));
        assert!(crate::check::check(&ws).errors().is_empty());

        assert!(ws.set_source(imported, "other = 42\n".into()));
        assert!(exports.contains_key("value"));
        assert!(!exports.contains_key("other"));
        assert!(!ws.modules[ws.root].scope.contains_key("value"));
        assert!(ws.modules[ws.root].scope.contains_key("other"));
        assert!(!crate::check::check(&ws).errors().is_empty());
    }

    /// The db text and the workspace text must always be the same text:
    /// spans come from the parsed db text while diagnostics are rendered
    /// against the workspace's, so a difference means the two phases are
    /// analyzing different programs (and can panic on a char boundary).
    fn assert_texts_agree(ws: &Workspace) {
        for (m, md) in ws.modules.iter().enumerate() {
            let file: SourceFile = *ws.inputs[m].file(&ws.db);
            assert_eq!(
                file.text(&ws.db),
                md.text.as_str(),
                "module {m} ({}) desynced",
                md.path.display()
            );
            assert_eq!(
                crate::lower::parse_module(&ws.db, file).module.defs.len(),
                md.module.defs.len(),
                "module {m} parsed differently in the two phases"
            );
        }
    }

    #[test]
    fn a_rejected_edit_leaves_both_texts_alone() {
        let mut ws = Workspace::from_source("q = 1\n");
        let root = ws.root;
        assert_texts_agree(&ws);
        // Changing the imports is refused, and the db must not keep the new
        // text: the workspace was not updated for it, so keeping it would
        // leave the two phases analyzing different programs.
        let longer = "import \"missing.cagara\"\nq = 1\nr = 2\n";
        assert!(!ws.set_source(root, longer.to_string()));
        assert_texts_agree(&ws);
        assert_eq!(ws.modules[root].text, "q = 1\n");
        // The workspace still works after the refused edit.
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        assert!(ws.set_source(root, "w = 9\n".to_string()));
        assert_texts_agree(&ws);
    }

    #[test]
    fn an_accepted_edit_keeps_both_texts_in_step() {
        let mut ws = Workspace::from_source("q = 1\n");
        let root = ws.root;
        // Growing, shrinking, and a multi-byte edit all stay in step.
        for text in [
            "q = 1\nr = 2\ns = 3\n",
            "q =\n",
            "q = \"héllo wörld\"\n",
            "x = 1\n",
        ] {
            assert!(ws.set_source(root, text.to_string()), "{text:?}");
            assert_texts_agree(&ws);
        }
        // Diagnostics render against the same text the span came from, so an
        // offset that is not a char boundary of it cannot panic.
        let _ = ws.diag(root, 7, "boom");
    }

    #[test]
    fn a_diagnostic_offset_inside_a_character_is_snapped() {
        // A span from a parse of another text can land inside a multi-byte
        // character; rendering must not panic.
        let ws = Workspace::from_source("q = \"héllo\"\n");
        let diag = ws.diag(ws.root, 7, "boom");
        assert_eq!(diag.line, 1, "{diag:?}");
        let diag = ws.diag(ws.root, "q = \"héllo\"\n".len() - 1, "boom");
        assert_eq!(diag.line, 1, "{diag:?}");
    }

    /// The cached line index must give the same line and column as counting
    /// newlines in the text directly, at every offset of a multi-line file
    /// with multi-byte characters and no trailing newline.
    #[test]
    fn diagnostic_lines_match_a_direct_count_at_every_offset() {
        let text = "a = 1\nb = \"héllo\"\nc = 3";
        let ws = Workspace::from_source(text);
        for offset in 0..=text.len() {
            if !text.is_char_boundary(offset) {
                continue;
            }
            let d = ws.diag(ws.root, offset, "x");
            let before = &text[..offset];
            let expected_line = before.matches('\n').count() + 1;
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            assert_eq!(d.line, expected_line, "line at offset {offset}");
            assert_eq!(d.col, offset - line_start + 1, "col at offset {offset}");
        }
    }

    /// An operator's fixity is a property of the language, read from the
    /// prelude when the table is built. A declaration anywhere else parses but
    /// cannot take effect, so it is reported rather than silently ignored.
    #[test]
    fn an_operator_declaration_outside_the_prelude_is_reported() {
        let ws = Workspace::from_source("infixl 1 &^\nq = 1\n");
        assert!(
            ws.diags
                .iter()
                .any(|d| d.message.contains("must be declared in `prelude.cagara`")),
            "{:?}",
            ws.diags
        );
        // The declaration is not a definition, so it produces no other error.
        assert_eq!(ws.diags.len(), 1, "{:?}", ws.diags);
        // And an ordinary file is unaffected.
        let ws = Workspace::from_source("q = 1\n");
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    }
}
