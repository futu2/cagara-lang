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
