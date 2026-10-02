//! Module loading: the embedded prelude, the root file, and its imports.
//! Parsing goes through the salsa `parse_module` query.

use crate::db::{Database, ModuleInput, SourceFile};
use crate::lower::{parse_module, ParsedModule};
use crate::resolve::{module_own, module_scope};
use crate::value::{EvalError, Prim};
use cagara_syntax::ast::{Import, Module, Span};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

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
    pub scope: HashMap<String, Binding>,
    /// This module's own definitions (what `import` and `alias.name` see).
    pub own: HashMap<String, Binding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        self.inputs.push(input);
        self.modules.push(LoadedModule {
            path,
            text,
            module: parsed.module,
            scope,
            own,
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
        let file = *self.inputs[m].file(&self.db);
        file.set_contents(&mut self.db, text.clone());
        let path = self.modules[m].path.clone();
        // Exports of `m` feed the scopes of its importers: refresh all
        // modules (the queries recompute only what changed).
        for k in 0..self.modules.len() {
            let input = self.inputs[k];
            self.modules[k].own = module_own(&self.db, input).clone();
            self.modules[k].scope = module_scope(&self.db, input).clone();
        }
        self.file_diags[m] = file_diags(&path, &text, &parsed, &self.modules[m].own);
        let md = &mut self.modules[m];
        md.text = text;
        md.module = parsed.module;
        self.rebuild_diags();
        true
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
        make_diag_range(&md.path, &md.text, offset, offset, message.into())
    }

    /// A diagnostic underlining `span` in module `m`.
    pub fn diag_span(&self, m: usize, span: Span, message: impl Into<String>) -> Diag {
        let md = &self.modules[m];
        diag_in(&md.path, &md.text, span, message)
    }

    pub fn eval_diag(&self, e: &EvalError) -> Diag {
        match e.span {
            Some(s) => self.diag_span(e.module, s, e.message.clone()),
            None => self.diag(e.module, 0, e.message.clone()),
        }
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
    let mut out: Vec<Diag> = parsed
        .errors
        .iter()
        .map(|e| make_diag(path, text, e.offset, format!("syntax error: {}", e.message)))
        .collect();
    // An operator's fixity is a property of the language, not of one module:
    // the table is built once, from the prelude. A declaration anywhere else
    // would silently do nothing (the file it appears in is already parsed by
    // the time it could be read), so say so instead.
    if path != Path::new(PRELUDE_PATH) {
        for d in &parsed.module.operators {
            out.push(make_diag(
                path,
                text,
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
        out.push(make_diag(
            path,
            text,
            parsed.module.defs[i].span.start as usize,
            msg,
        ));
    }
    out
}

/// A diagnostic underlining `span` in the file `path` with contents `text`.
pub fn diag_in(path: &Path, text: &str, span: Span, message: impl Into<String>) -> Diag {
    make_diag_range(
        path,
        text,
        span.start as usize,
        span.end as usize,
        message.into(),
    )
}

fn make_diag(path: &Path, text: &str, offset: usize, message: String) -> Diag {
    make_diag_range(path, text, offset, offset, message)
}

fn make_diag_range(path: &Path, text: &str, start: usize, end: usize, message: String) -> Diag {
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
    let before = &text[..start];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let line = before.matches('\n').count() + 1;
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
