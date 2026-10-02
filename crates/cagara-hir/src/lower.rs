//! Salsa queries from source text to the owned AST.
//!
//! Parsing is memoized per `SourceFile`: editing one file re-parses only that
//! file, and an unchanged AST backdates so downstream queries are reused.

use crate::db::SourceFile;
use cagara_syntax::ast::{self, Module};
use cagara_syntax::ParseError;

/// Parsed and lowered module plus recovered syntax errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedModule {
    pub module: Module,
    pub errors: Vec<ParseError>,
}

/// Parse and lower one file (memoized).
#[salsa::tracked(returns(ref))]
pub(crate) fn parse_module(db: &dyn salsa::Database, file: SourceFile) -> ParsedModule {
    let (module, errors) = ast::lower_source(file.text(db));
    ParsedModule { module, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use salsa::Setter;

    #[test]
    fn edit_reparses_changed_file() {
        let mut db = Database::default();
        let file = SourceFile::new(&db, "x = 1\n".to_string());
        assert_eq!(parse_module(&db, file).module.defs.len(), 1);
        file.set_text(&mut db).to("x = 1\ny = 2\n".to_string());
        assert_eq!(parse_module(&db, file).module.defs.len(), 2);
    }
}
