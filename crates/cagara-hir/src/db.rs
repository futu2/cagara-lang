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
    /// Replace this file's text.
    ///
    /// This changes what the queries parse but nothing else, so a
    /// [`Workspace`](crate::workspace::Workspace) that holds this file then
    /// disagrees with the db: spans come from the db text while diagnostics
    /// are rendered against the workspace's, and the two phases can analyze
    /// different programs. Use [`Workspace::set_source`] instead, which
    /// updates both. This is for tests that drive the db directly and for
    /// `set_source` itself.
    pub fn set_contents(&self, db: &mut Database, text: String) {
        self.set_text(db).to(text);
    }
}

/// Input: one loaded module: its file and the modules its imports resolved
/// to (loading files is outside salsa). Its exports and scope are the
/// `module_own` / `module_scope` queries, and its definitions come from
/// `parse_module(file)`, so an edit recomputes only what depends on it.
#[salsa::input]
pub struct ModuleInput {
    pub index: usize,
    #[returns(ref)]
    pub path: std::path::PathBuf,
    pub file: SourceFile,
    /// The prelude (`None` for the prelude itself).
    pub prelude: Option<ModuleInput>,
    /// `import "path" [as alias]`, in source order.
    #[returns(ref)]
    pub imports: Vec<(Option<String>, ModuleInput)>,
}
