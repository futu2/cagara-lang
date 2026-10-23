#[salsa::db]
#[derive(Clone, Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Database {}

/// The parsed form of one definition, as a salsa input value.
///
/// The AST rather than its text, because a definition parsed on its own would
/// index its own slice of the file: every span-based consumer — diagnostics,
/// hover, goto, the typed tree — would have to add an offset back on. Loading
/// and parsing stay outside salsa, exactly as they already are for files, and
/// only the *check* becomes per definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefAst(pub cagara_syntax::ast::Def);

// SAFETY: a `Def` owns its data — names and literals are `String`s and spans
// are offsets into text the workspace keeps — so it carries no reference whose
// target a later revision could move.
unsafe impl salsa::SalsaValue for DefAst {}

/// Input: source file content
#[salsa::input]
pub struct SourceFile {
    #[returns(deref)]
    pub text: String,
}

/// Input: one definition of a module.
///
/// Split out of the file so that checking is memoized per definition: an edit
/// inside one definition leaves every other `DefInput` alone, and with it every
/// check that never read the edited one.
#[salsa::input]
pub struct DefInput {
    /// The name this definition introduces.
    ///
    /// A field of its own, not a field of `def`, so that name resolution —
    /// which needs only names — stays valid across an edit to a body. Reading
    /// `def` would make the module's whole scope depend on every body.
    #[returns(ref)]
    pub name: String,
    #[returns(ref)]
    pub def: DefAst,
}

/// Input: one loaded module: its file, the modules its imports resolved to
/// (loading files is outside salsa), and its definitions.
///
/// Its exports and scope are the `module_own` / `module_scope` queries, and
/// each definition is checked by `def_check`, so an edit inside one definition
/// recomputes only what read it.
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
    /// The module's definitions, in source order (`parse_module(file)` has
    /// them too; these are the copies the per-definition checks read).
    #[returns(ref)]
    pub defs: Vec<DefInput>,
}
