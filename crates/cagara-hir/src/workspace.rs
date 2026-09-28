//! Module loading: the embedded prelude, the root file, and its imports.
//! Parsing goes through the salsa `parse_module` query.

use crate::db::{Database, SourceFile};
use crate::lower::parse_module;
use crate::value::{EvalError, Prim, PRIMS};
use cagara_syntax::ast::{Import, Module};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const PRELUDE_SRC: &str = include_str!("../../../prelude.cagara");
pub const PRELUDE_PATH: &str = "<prelude>";

#[derive(Debug, Clone, Copy)]
pub enum Binding {
    /// (module index, definition index)
    Def(usize, usize),
    Module(usize),
    /// `__` primitives; visible only inside the prelude.
    Prim(Prim),
}

pub struct LoadedModule {
    pub path: PathBuf,
    pub text: String,
    pub module: Module,
    pub scope: HashMap<String, Binding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub path: String,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}: error: {}", self.path, self.line, self.col, self.message)
    }
}

pub struct Workspace {
    pub db: Database,
    pub files: HashMap<PathBuf, SourceFile>,
    pub modules: Vec<LoadedModule>,
    pub root: usize,
    pub diags: Vec<Diag>,
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
        ws
    }

    pub fn open(path: &Path) -> Self {
        let mut ws = Self::empty();
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        match std::fs::read_to_string(&p) {
            Ok(text) => ws.root = ws.add(p, text),
            Err(e) => ws.diags.push(Diag {
                path: p.display().to_string(),
                line: 1,
                col: 1,
                message: format!("cannot read file: {e}"),
            }),
        }
        ws
    }

    fn add(&mut self, path: PathBuf, text: String) -> usize {
        self.stack.push(path.clone());
        let file = SourceFile::new(&self.db, text.clone());
        self.files.insert(path.clone(), file);
        let parsed = parse_module(&self.db, file).clone();
        for e in &parsed.errors {
            self.diags.push(make_diag(&path, &text, e.offset, format!("syntax error: {}", e.message)));
        }

        let mut scope = HashMap::new();
        if self.modules.is_empty() {
            for (n, p) in PRIMS {
                scope.insert(n.to_string(), Binding::Prim(*p));
            }
        } else {
            for (i, d) in self.modules[0].module.defs.iter().enumerate() {
                scope.insert(d.name.clone(), Binding::Def(0, i));
            }
        }

        for imp in &parsed.module.imports {
            let Some(target) = self.import(&path, &text, imp) else { continue };
            match &imp.alias {
                Some(a) => {
                    scope.insert(a.clone(), Binding::Module(target));
                }
                None => {
                    for (i, d) in self.modules[target].module.defs.iter().enumerate() {
                        scope.insert(d.name.clone(), Binding::Def(target, i));
                    }
                }
            }
        }

        let m = self.modules.len();
        let mut seen = HashSet::new();
        for (i, d) in parsed.module.defs.iter().enumerate() {
            if seen.insert(d.name.clone()) {
                scope.insert(d.name.clone(), Binding::Def(m, i));
            } else {
                let msg = format!("`{}` is defined twice (overloading is not supported yet)", d.name);
                self.diags.push(make_diag(&path, &text, d.span.start as usize, msg));
            }
        }

        self.stack.pop();
        self.by_path.insert(path.clone(), m);
        self.modules.push(LoadedModule { path, text, module: parsed.module, scope });
        m
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
            self.diags.push(make_diag(from, text, offset, msg));
            return None;
        }
        match std::fs::read_to_string(&target) {
            Ok(t) => Some(self.add(target, t)),
            Err(e) => {
                let msg = format!("cannot import `{}`: {e}", imp.path);
                self.diags.push(make_diag(from, text, offset, msg));
                None
            }
        }
    }

    pub fn diag(&self, m: usize, offset: usize, message: impl Into<String>) -> Diag {
        let md = &self.modules[m];
        make_diag(&md.path, &md.text, offset, message.into())
    }

    pub fn eval_diag(&self, e: &EvalError) -> Diag {
        self.diag(e.module, e.span.map_or(0, |s| s.start as usize), e.message.clone())
    }
}

fn make_diag(path: &Path, text: &str, offset: usize, message: String) -> Diag {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let col = offset - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    Diag { path: path.display().to_string(), line, col, message }
}
