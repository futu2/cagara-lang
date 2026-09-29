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
        write!(f, "{}:{}:{}: error: {}", self.path, self.line, self.col, self.message)?;
        if !self.source.is_empty() {
            let n = self.line.to_string();
            let pad = self.source.get(..self.col - 1).map_or(0, |s| s.chars().count());
            let gutter = " ".repeat(n.len());
            write!(f, "\n {n} | {}\n {gutter} | {}{}", self.source, " ".repeat(pad), "^".repeat(self.width.max(1)))?;
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
        let mut ws = Self::empty();
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        match std::fs::read_to_string(&p) {
            Ok(text) => ws.root = ws.add(p, text),
            Err(e) => ws.import_diags.push(Diag {
                path: p.display().to_string(),
                line: 1,
                col: 1,
                message: format!("cannot read file: {e}"),
                source: String::new(),
                width: 0,
            }),
        }
        ws.rebuild_diags();
        ws
    }

    fn add(&mut self, path: PathBuf, text: String) -> usize {
        self.stack.push(path.clone());
        let file = SourceFile::new(&self.db, text.clone());
        self.files.insert(path.clone(), file);
        let parsed = parse_module(&self.db, file).clone();

        let prelude = if self.modules.is_empty() { None } else { Some(self.inputs[0]) };
        let mut imports = Vec::new();
        for imp in &parsed.module.imports {
            let Some(target) = self.import(&path, &text, imp) else { continue };
            imports.push((imp.alias.clone(), self.inputs[target]));
        }

        self.stack.pop();
        let m = self.modules.len();
        self.by_path.insert(path.clone(), m);
        let input = ModuleInput::new(&self.db, m, path.clone(), file, prelude, imports);
        let own = module_own(&self.db, input).clone();
        let scope = module_scope(&self.db, input).clone();
        self.file_diags.push(file_diags(&path, &text, &parsed, &own));
        self.inputs.push(input);
        self.modules.push(LoadedModule { path, text, module: parsed.module, scope, own });
        m
    }

    /// Replace the text of loaded module `m`. Parsing, name resolution, and
    /// type checking are salsa queries, so only what depends on the file is
    /// recomputed. Returns `false` if its imports changed; the workspace
    /// must then be reloaded, since loading files is outside salsa.
    pub fn set_source(&mut self, m: usize, text: String) -> bool {
        let imports = |md: &Module| md.imports.iter().map(|i| (i.path.clone(), i.alias.clone())).collect::<Vec<_>>();
        let before = imports(&self.modules[m].module);
        let file = *self.inputs[m].file(&self.db);
        file.set_contents(&mut self.db, text.clone());
        let parsed = parse_module(&self.db, file).clone();
        if imports(&parsed.module) != before {
            return false;
        }
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
        self.diags = self.import_diags.iter().chain(self.file_diags.iter().flatten()).cloned().collect();
    }

    fn import(&mut self, from: &Path, text: &str, imp: &Import) -> Option<usize> {
        let base = from.parent().unwrap_or(Path::new("."));
        let raw = base.join(&imp.path);
        let target = raw.canonicalize().unwrap_or(raw);
        if let Some(&i) = self.by_path.get(&target) {
            return Some(i);
        }
        let offset = imp.span.start as usize;
        if self.stack.contains(&target) {
            let msg = format!("import cycle: `{}` is already being loaded", imp.path);
            self.import_diags.push(make_diag(from, text, offset, msg));
            return None;
        }
        match std::fs::read_to_string(&target) {
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

/// Syntax errors of a file, and overloads missing a signature.
fn file_diags(path: &Path, text: &str, parsed: &ParsedModule, own: &HashMap<String, Binding>) -> Vec<Diag> {
    let mut out: Vec<Diag> = parsed
        .errors
        .iter()
        .map(|e| make_diag(path, text, e.offset, format!("syntax error: {}", e.message)))
        .collect();
    let mut missing: Vec<(usize, &str)> = Vec::new();
    for (name, b) in own {
        if let Binding::Overloads(_, is) = b {
            missing.extend(is.iter().filter(|&&i| parsed.module.defs[i].ty.is_none()).map(|&i| (i, name.as_str())));
        }
    }
    missing.sort();
    for (i, name) in missing {
        let msg = format!("`{name}` is defined more than once, so each definition needs a type signature (overloading)");
        out.push(make_diag(path, text, parsed.module.defs[i].span.start as usize, msg));
    }
    out
}

/// A diagnostic underlining `span` in the file `path` with contents `text`.
pub fn diag_in(path: &Path, text: &str, span: Span, message: impl Into<String>) -> Diag {
    make_diag_range(path, text, span.start as usize, span.end as usize, message.into())
}

fn make_diag(path: &Path, text: &str, offset: usize, message: String) -> Diag {
    make_diag_range(path, text, offset, offset, message)
}

fn make_diag_range(path: &Path, text: &str, start: usize, end: usize, message: String) -> Diag {
    let start = start.min(text.len());
    let end = end.clamp(start, text.len());
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
