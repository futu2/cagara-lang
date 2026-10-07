use super::*;

fn assert_matches_fresh(ws: &Workspace) -> Compilation {
    let cached = compile(ws);
    let fresh = compile_checked(ws, &crate::check::check(ws));
    assert_eq!(cached, fresh);
    cached
}

#[test]
fn cached_compilation_matches_fresh_across_edits_errors_and_overloads() {
    let table = "users : query { a = int } = table \"s\" \"one\"\n";
    let query = "q = users & select { v = .a + 1 }\n";
    let initial = format!("{table}{query}");
    let mut ws = Workspace::from_source(&initial);
    assert!(assert_matches_fresh(&ws).is_ok());
    for source in [
        initial.replace("one", "two"),
        format!("{table}alias = users\nq = alias & select {{ v = .a + 1 }}\n"),
        initial.replace("a = int", "a = float"),
        initial.replace("a = int", "a = bool"),
        initial.replace(".a + 1", ".missing + 1"),
        format!("# héllo\n{initial}"),
        "q = (\n".into(),
        initial.clone(),
        format!("{table}{table}{query}"),
        format!("{table}q = 1\n"),
        String::new(),
        initial,
    ] {
        assert!(ws.set_source(ws.root, source));
        assert_matches_fresh(&ws);
    }
}

#[test]
fn imported_dependency_edits_and_locations_match_fresh_compilation() {
    let root_path = std::path::PathBuf::from("/tmp/cagara-facts/main.cagara");
    let lib_path = std::path::PathBuf::from("/tmp/cagara-facts/lib.cagara");
    let root = "import \"lib.cagara\" as lib\nq = lib.value\n";
    let lib = "value : query { a = int } = table \"s\" \"one\"\n";
    let buffers = HashMap::from([(lib_path.clone(), lib.to_string())]);
    let mut ws = Workspace::open_with_buffers(&root_path, root.into(), &buffers);
    assert!(assert_matches_fresh(&ws).is_ok());
    let module = ws.module_for_path(&lib_path).unwrap();
    for source in [
        lib.replace("one", "two"),
        format!("# moved\n{lib}"),
        "value = false\n".into(),
        lib.into(),
    ] {
        assert!(ws.set_source(module, source));
        assert_matches_fresh(&ws);
    }
    // Several accepted edits before the next request still compare with
    // the last completed compilation.
    assert!(ws.set_source(module, lib.replace("one", "two")));
    assert!(ws.set_source(module, lib.replace("one", "six")));
    assert_matches_fresh(&ws);
}

#[test]
fn transitive_users_change_while_an_independent_query_is_reused() {
    let src = "base : query { a = int } = table \"s\" \"one\"\n\
               alias = base\nq = alias\n\
               alone : query { a = int } = table \"s\" \"own\"\n";
    let mut ws = Workspace::from_source(src);
    assert!(compile(&ws).is_ok());
    let before = crate::elaborate::elaborated_defs().len();
    assert!(ws.set_source(ws.root, src.replace("\"one\"", "\"two\"")));
    let edited = compile(&ws);
    assert_eq!(
        &crate::elaborate::elaborated_defs()[before..],
        &[(ws.root, 0), (ws.root, 1), (ws.root, 2)]
    );
    assert_eq!(edited, compile_checked(&ws, &crate::check::check(&ws)));
}

#[test]
fn dependency_walk_handles_long_chains_and_cycles_without_recursing() {
    let mut source = "d0 = 1\n".to_string();
    for i in 1..10_000 {
        source.push_str(&format!("d{i} = d{}\n", i - 1));
    }
    let ws = Workspace::from_source(&source);
    assert!(compile(&ws).is_ok());
    let mut ws = Workspace::from_source("a = b\nb = a\n");
    assert_matches_fresh(&ws);
    assert!(ws.set_source(ws.root, "a = b\nb = 1\n".into()));
    assert_matches_fresh(&ws);
}

#[test]
fn cached_relations_keep_current_source_locations() {
    let src = "q : query { a = int } = table \"public\" \"items\"\n";
    let mut ws = Workspace::from_source(src);
    assert!(compile(&ws).is_ok());
    assert!(ws.set_source(ws.root, format!("# moved\n{src}")));
    let cached = compile(&ws);
    let fresh = compile_checked(&ws, &crate::check::check(&ws));
    assert_eq!(
        cached, fresh,
        "cache reuse must preserve every Rel::At span"
    );
}

#[test]
fn overloaded_query_definitions_keep_source_identity() {
    let ws = Workspace::from_source(
        "q : query { a = int } = table \"s\" \"one\"\n\
         q : query { a = int } = table \"s\" \"two\"\n",
    );
    let tc = crate::check::check(&ws);
    let compilation = compile_checked(&ws, &tc);
    assert!(compilation.diagnostics.is_empty());
    let tables: Vec<_> = compilation
        .queries
        .into_iter()
        .map(|query| match query.result.unwrap() {
            Rel::At(_, rel) => match *rel {
                Rel::Table { name, .. } => name,
                other => panic!("expected table, got {other:?}"),
            },
            other => panic!("expected located table, got {other:?}"),
        })
        .collect();
    assert_eq!(tables, vec!["one", "two"]);

    let compilation = compile(&ws);
    assert_eq!(
        compilation
            .queries
            .iter()
            .map(|query| query.id.def)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_ne!(
        compilation.queries[0].key, compilation.queries[1].key,
        "duplicate definitions need distinct cache identities"
    );
}

#[test]
fn definition_keys_survive_unrelated_insertions() {
    let mut ws = Workspace::from_source(
        "first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n",
    );
    let before = compile(&ws);
    let keys_before: Vec<_> = before.queries.iter().map(|query| query.key).collect();

    assert!(ws.set_source(
        ws.root,
        "inserted : query { a = int } = table \"public\" \"inserted\"\n\
         first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n"
            .into()
    ));
    let after = compile(&ws);
    let by_name: HashMap<_, _> = after
        .queries
        .iter()
        .map(|query| (query.name.as_str(), query.key))
        .collect();

    assert_eq!(by_name["first"], keys_before[0]);
    assert_eq!(by_name["second"], keys_before[1]);
}

#[test]
fn inserting_a_definition_refreshes_moved_query_locations() {
    let mut ws = Workspace::from_source(
        "first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n",
    );
    compile(&ws);
    let before = crate::elaborate::elaborated_defs().len();

    assert!(ws.set_source(
        ws.root,
        "inserted : query { a = int } = table \"public\" \"inserted\"\n\
         first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n"
            .into()
    ));
    let after = compile(&ws);

    assert!(after.diagnostics.is_empty(), "{:?}", after.diagnostics);
    assert_eq!(
        &crate::elaborate::elaborated_defs()[before..],
        &[(ws.root, 0), (ws.root, 1), (ws.root, 2)],
        "moved queries need new source locations"
    );
    assert_eq!(after, compile_checked(&ws, &crate::check::check(&ws)));
}

#[test]
fn changing_a_dependency_reelaborates_its_users() {
    let mut ws = Workspace::from_source(
        "base : query { a = int } = table \"public\" \"base\"\n\
         q = base\n",
    );
    let first = compile(&ws);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let before = crate::elaborate::elaborated_defs().len();

    assert!(ws.set_source(
        ws.root,
        "base : query { a = int } = table \"public\" \"changed\"\n\
         q = base\n"
            .into()
    ));
    let edited = compile(&ws);

    assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
    assert_eq!(
        &crate::elaborate::elaborated_defs()[before..],
        &[(ws.root, 0), (ws.root, 1)],
        "a dependent definition must not reuse a stale expanded query"
    );
}

#[test]
fn editing_one_definition_changes_only_its_key() {
    let mut ws = Workspace::from_source(
        "first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n",
    );
    let before = compile(&ws);
    let first_before = before.queries[0].key;
    let second_before = before.queries[1].key;

    assert!(ws.set_source(
        ws.root,
        "first : query { a = int } = table \"public\" \"changed\"\n\
         second : query { a = int } = table \"public\" \"second\"\n"
            .into()
    ));
    let after = compile(&ws);

    assert_ne!(after.queries[0].key, first_before);
    assert_eq!(after.queries[1].key, second_before);
}

#[test]
fn compiler_input_is_a_read_only_view_of_the_workspace() {
    let ws = Workspace::from_source("q = 1\n");
    let tc = crate::check::check(&ws);
    let input = CompilerInput::new(&ws, &tc);
    let root = input.root();

    assert_eq!(root.index(), ws.root);
    assert_eq!(root.path(), Path::new("<input>"));
    assert_eq!(root.text(), "q = 1\n");
    assert_eq!(root.source().defs[0].name, "q");
    assert!(root.scope().contains_key("q"));

    assert_eq!(compile_input(input), compile_checked(&ws, &tc));
}

#[test]
fn compiler_input_preserves_source_diagnostics() {
    let ws = Workspace::from_source("q = (\n");
    let tc = crate::check::check(&ws);
    let compilation = compile_input(CompilerInput::new(&ws, &tc));

    assert!(!compilation.diagnostics.is_empty());
    assert_eq!(compilation.diagnostics, ws.diags);
}

#[test]
fn diagnostics_reuse_the_cached_compilation() {
    let ws = Workspace::from_source("q = (\n");

    let first = compile_diagnostics(&ws);
    let after_first = crate::elaborate::elaborate_runs();
    let second = compile_diagnostics(&ws);

    assert_eq!(second, first);
    assert_eq!(
        crate::elaborate::elaborate_runs(),
        after_first,
        "cached diagnostics should not re-elaborate the source"
    );
}

#[test]
fn compilation_cache_reuses_and_invalidates_elaboration() {
    let mut ws = Workspace::from_source("q : query { a = int } = table \"public\" \"items\"\n");

    let first = compile(&ws);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let after_first = crate::elaborate::elaborate_runs();

    let second = compile(&ws);
    assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
    assert_eq!(
        crate::elaborate::elaborate_runs(),
        after_first,
        "unchanged compilation should reuse the cached result"
    );

    assert!(ws.set_source(
        ws.root,
        "q : query { a = int } = table \"public\" \"updated\"\n".into()
    ));
    let edited = compile(&ws);
    assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
    assert_eq!(
        crate::elaborate::elaborate_runs(),
        after_first + 1,
        "an accepted edit should invalidate the cached compilation"
    );
}

#[test]
fn changing_the_compilation_root_invalidates_the_cache() {
    let root_path = std::path::PathBuf::from("/tmp/cagara-root-cache/root.cagara");
    let lib_path = std::path::PathBuf::from("/tmp/cagara-root-cache/lib.cagara");
    let root_src = "import \"lib.cagara\" as lib\nq = lib.value\n";
    let lib_src = "value : query { a = int } = table \"public\" \"items\"\n";
    let mut buffers = std::collections::HashMap::new();
    buffers.insert(root_path.clone(), root_src.to_string());
    buffers.insert(lib_path.clone(), lib_src.to_string());
    let mut ws = Workspace::open_with_buffers(&root_path, root_src.to_string(), &buffers);

    let first = compile(&ws);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let after_first = crate::elaborate::elaborate_runs();

    assert!(ws.set_root_path(&lib_path));
    let second = compile(&ws);
    assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
    assert_eq!(
        crate::elaborate::elaborate_runs(),
        after_first + 1,
        "changing roots must not reuse the previous root compilation"
    );
}

#[test]
fn editing_one_definition_reuses_unchanged_root_definitions() {
    let mut ws = Workspace::from_source(
        "first : query { a = int } = table \"public\" \"first\"\n\
         second : query { a = int } = table \"public\" \"second\"\n",
    );

    let first = compile(&ws);
    assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
    let before = crate::elaborate::elaborated_defs().len();

    assert!(ws.set_source(
        ws.root,
        "first : query { a = int } = table \"public\" \"other\"\n\
         second : query { a = int } = table \"public\" \"second\"\n"
            .into()
    ));
    let edited = compile(&ws);
    assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
    assert_eq!(
        &crate::elaborate::elaborated_defs()[before..],
        &[(ws.root, 0)],
        "an unchanged definition should reuse its erased query"
    );
}
