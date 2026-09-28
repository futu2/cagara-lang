//! Salsa queries from source text to the owned AST.
//!
//! Parsing is memoized per `SourceFile`: editing one file re-parses only that
//! file, and an unchanged AST backdates so downstream queries are reused.

use crate::db::SourceFile;
use crate::diagnostics::Diagnostic;
use cagara_syntax::ast::{self, Module};
use cagara_syntax::ParseError;
use salsa::Accumulator;

/// Parsed and lowered module plus recovered syntax errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModule {
    pub module: Module,
    pub errors: Vec<ParseError>,
}

/// Parse and lower one file (memoized).
#[salsa::tracked(returns(ref))]
pub fn parse_module(db: &dyn salsa::Database, file: SourceFile) -> ParsedModule {
    let (module, errors) = ast::lower_source(file.text(db));
    ParsedModule { module, errors }
}

/// Report syntax errors for a file as accumulated diagnostics.
#[salsa::tracked]
pub fn lower_file(db: &dyn salsa::Database, file: SourceFile) {
    let parsed = parse_module(db, file);
    for e in &parsed.errors {
        Diagnostic::new(format!("syntax error: {}", e.message))
            .with_span(e.offset, e.offset)
            .accumulate(db);
    }
    for d in &parsed.module.defs {
        if matches!(d.body.kind, ast::ExprKind::Error) {
            Diagnostic::new(format!("definition '{}' has no valid body", d.name))
                .with_span(d.span.start as usize, d.span.end as usize)
                .accumulate(db);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use salsa::Setter;

    #[test]
    fn valid_file_has_no_diagnostics() {
        let db = Database::default();
        let file = SourceFile::new(&db, "x : int = 42\n".to_string());
        lower_file(&db, file);
        let diags = lower_file::accumulated::<Diagnostic>(&db, file);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(parse_module(&db, file).module.defs[0].name, "x");
    }

    #[test]
    fn missing_body_is_reported() {
        let db = Database::default();
        let file = SourceFile::new(&db, "x : int\n".to_string());
        lower_file(&db, file);
        let diags = lower_file::accumulated::<Diagnostic>(&db, file);
        assert!(!diags.is_empty());
    }

    #[test]
    fn edit_reparses_changed_file() {
        let mut db = Database::default();
        let file = SourceFile::new(&db, "x = 1\n".to_string());
        assert_eq!(parse_module(&db, file).module.defs.len(), 1);
        file.set_text(&mut db).to("x = 1\ny = 2\n".to_string());
        assert_eq!(parse_module(&db, file).module.defs.len(), 2);
    }
}
