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
