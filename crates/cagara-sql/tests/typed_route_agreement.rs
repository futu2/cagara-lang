//! Differential SQL coverage for the typed query lowering migration.
//!
//! The HIR tests compare checked relations. This test adds the backend's
//! observable result: every successful query in the shipped examples and HIR
//! corpus must emit the same SQL through the production route and through an
//! independently normalized term route.

use cagara_hir::term_oracle::{expression, lower_query, Body, Facts, Normalizer, WorkspaceFacts};
use cagara_hir::{check, erase, root_queries_checked, Workspace};
use cagara_sql::{compile, Dialect, Options};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn corpus_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in [
        root.join("examples"),
        root.join("crates/cagara-hir/tests/corpus"),
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            files.extend(
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.extension().is_some_and(|ext| ext == "cagara")),
            );
        }
    }
    files.sort();
    files
}

fn term_relation(ws: &Workspace, tc: &cagara_hir::TypeCheck, def: usize) -> cagara_hir::ir::Rel {
    let facts = WorkspaceFacts::new(ws, tc);
    let body = Body {
        module: ws.root,
        def,
        expr: &ws.modules[ws.root].module.defs[def].body,
    };
    let resolved = |module: usize, id: u32| facts.resolution(module, id);
    let mut normalizer = Normalizer::new(Box::new(resolved));
    let term = normalizer
        .in_module(ws.root, |cx| expression(&facts, cx, &body))
        .unwrap_or_else(|gap| {
            panic!(
                "{}: definition {def} did not normalize: {}",
                ws.modules[ws.root].path.display(),
                gap.message
            )
        });
    let checked = lower_query(&term).unwrap_or_else(|gap| {
        panic!(
            "{}: definition {def} did not lower: {}",
            ws.modules[ws.root].path.display(),
            gap.message
        )
    });
    erase(checked).unwrap_or_else(|error| {
        panic!(
            "{}: definition {def} did not erase: {}",
            ws.modules[ws.root].path.display(),
            error.message
        )
    })
}

#[test]
fn corpus_typed_route_emits_the_same_sql_as_the_term_route() {
    let dialects = [
        Dialect::Ansi,
        Dialect::Postgres,
        Dialect::Sqlite,
        Dialect::DuckDb,
        Dialect::Mysql,
        Dialect::Tsql,
    ];
    let mut compared = 0usize;
    for path in corpus_files() {
        let ws = Workspace::open(&path);
        let tc = check(&ws);
        let production = root_queries_checked(&ws, &tc);
        for (name, result) in production {
            let Ok(production_rel) = result else {
                continue;
            };
            let def = ws.modules[ws.root]
                .module
                .defs
                .iter()
                .position(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("{}: missing definition `{name}`", path.display()));
            let term_rel = term_relation(&ws, &tc, def);
            for dialect in dialects {
                let options = Options {
                    dialect,
                    pretty: false,
                    optimize: false,
                };
                let production_sql = compile(&production_rel, options);
                let term_sql = compile(&term_rel, options);
                assert_eq!(
                    production_sql, term_sql,
                    "{path:?}, definition `{name}`, dialect {dialect:?}"
                );
            }
            compared += 1;
        }
    }
    assert!(
        compared >= 20,
        "only {compared} corpus queries were compared"
    );
}
