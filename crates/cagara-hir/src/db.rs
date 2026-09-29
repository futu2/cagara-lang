use salsa::Setter;

#[salsa::db]
#[derive(Clone, Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Database {}

/// Input: source file content
#[salsa::input]
pub struct SourceFile {
    #[returns(deref)]
    pub text: String,
}

impl SourceFile {
    pub fn set_contents(&self, db: &mut Database, text: String) {
        self.set_text(db).to(text);
    }
}

/// Input: one loaded module. `scope` and `owns` come from name resolution
/// in the workspace; `deps` are the modules it imports (and the prelude).
/// Its definitions are read through `parse_module(file)`, so editing a file
/// invalidates only the checks that depend on it.
#[salsa::input]
pub struct ModuleInput {
    pub index: usize,
    #[returns(ref)]
    pub path: std::path::PathBuf,
    pub file: SourceFile,
    #[returns(ref)]
    pub scope: std::collections::HashMap<String, crate::workspace::Binding>,
    /// Exports of every module loaded up to this one (for `alias.name`).
    #[returns(ref)]
    pub owns: Vec<std::collections::HashMap<String, crate::workspace::Binding>>,
    #[returns(ref)]
    pub deps: Vec<ModuleInput>,
}
