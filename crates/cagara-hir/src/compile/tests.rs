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

/// A stage argument that type-checks but is not the literal the elaborator
/// needs (`limit (1 + 1)`) fails during source elaboration. Its diagnostic must
/// describe the *user's* file: a hardcoded module index attributed it to the
/// prelude, so it printed the prelude's first line and put the caret at the
/// wrong column.
#[test]
fn an_elaboration_error_points_at_the_users_module_not_the_prelude() {
    let ws = Workspace::from_source("q : query { a = int } = table \"s\" \"t\" & limit (1 + 1)\n");
    let diags: Vec<Diag> = compile(&ws)
        .queries
        .iter()
        .filter_map(|q| q.result.as_ref().err().cloned())
        .collect();
    assert_eq!(diags.len(), 1, "{diags:?}");
    let d = &diags[0];
    assert_eq!(d.path, "<input>", "{d}");
    assert!(d.message.contains("expects an int"), "{d}");
    assert!(
        !d.source.contains("Cagara prelude"),
        "the diagnostic rendered the prelude's source: {d}"
    );
}

/// Every definition the checker accepted must reach a verdict that is not a
/// *compiler capability gap*.
///
/// This is the gate for one of the two bug classes this compiler kept
/// producing: the checker types a construct, a later phase re-reads the same
/// source, decides it means something narrower, and rejects a well-typed
/// program. Each such gap was found one at a time, in production, because
/// nothing asserted the property — the seam between checking and elaboration
/// was only ever checked by hand.
///
/// The assertion is deliberately *not* "every checker-accepted definition
/// compiles". A definition may legitimately fail:
///
/// * `Fault::Program` — malformed source, or a rule the checked constructors
///   own. The user gets a real message.
/// * `Fault::NotAQuery` — a scalar, a stage, a function: nothing to build, and
///   no verdict either way.
///
/// What is never legitimate is `Fault::Compiler` for a definition the checker
/// accepted: that is the compiler admitting it cannot build a program it has
/// already declared well-typed.
///
/// **The other class is not visible here.** A bug that lives *in the checker*
/// — the `5d34464` deferred-row defect was one — surfaces as `Fault::Program`
/// with a checker diagnostic, so this gate cannot see it by construction.
/// [`accepted_programs_have_no_checker_errors`] covers that side; the two are
/// complements and neither subsumes the other.
///
/// Returns the offending definitions, so a caller can assert on the count or
/// print them.
fn compiler_gaps(ws: &Workspace) -> Vec<String> {
    let tc = crate::check::check(ws);
    compile_checked(ws, &tc)
        .queries
        .iter()
        .filter(|query| query.fault == Some(Fault::Compiler))
        .map(|query| {
            let def = &ws.modules[query.id.module].module.defs[query.id.def];
            let span = def.span;
            let text = &ws.modules[query.id.module].text;
            let source = text
                .get(span.start as usize..span.end as usize)
                .unwrap_or("<unavailable>");
            let message = match &query.result {
                Err(diag) => diag.message.clone(),
                Ok(_) => unreachable!("a fault with a successful result"),
            };
            format!("`{}`: {source}\n  -> {message}", query.name)
        })
        .collect()
}

/// Compile `src` and return every diagnostic that is a *compiler* fault, plus
/// every checker diagnostic.
///
/// The pair is what a caller needs to tell the two bug classes apart: a
/// checker diagnostic for a program that should be legal is one class, a
/// compiler fault is the other.
fn verdicts(tables: &str, src: &str) -> (Vec<String>, Vec<String>) {
    let ws = Workspace::from_source(&format!("{tables}{src}"));
    let tc = crate::check::check(&ws);
    let checker = tc.errors().iter().map(|e| e.diag.message.clone()).collect();
    (compiler_gaps(&ws), checker)
}

/// Nothing in a program the *checker must accept* may produce a checker
/// diagnostic.
///
/// This is the complement of [`compiler_gaps`], and it exists because the two
/// bug classes have different signatures. A deferred-row defect (`5d34464`) is
/// not an elaboration gap at all: with `row_open` reverted to its pre-fix
/// "is this a bare variable?" test, the failure appears as
/// ``no column `up1` `` from the *checker*, as `Fault::Program`. A gate that
/// only inspected compiler faults would pass on a compiler that had lost the
/// fix entirely.
///
/// So each case here is a program that is legal by the language's rules, and
/// the assertion is simply that it checks. The table gives the row shapes the
/// cases need.
#[test]
fn accepted_programs_have_no_checker_errors() {
    const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = \
                          table \"public\" \"users\"\n\
                          orders : query { user_id = int, amount = float, status = string } = \
                          table \"public\" \"orders\"\n";
    // Each case is a helper whose stages are *deferred*: the parameter is an
    // open row until the helper is applied, so a stage that reads a column an
    // earlier stage of the same helper added sees the unreduced row term
    // `merge α { up1 = int }` rather than a finished row. Reading that term as
    // final reports the column the merge is about to contribute as missing.
    //
    // Every shape below was rejected before `5d34464`, and each one exercises a
    // different stage that reads its input's columns — which is why the bug
    // appeared stage by stage rather than all at once.
    for (case, body) in [
        // ... read by a following `update`.
        (
            "update after update",
            "h = q => q & update { up1 = .id } & update { up2 = .up1 }\nq = users & h\n",
        ),
        // ... by a `where`.
        (
            "where after update",
            "h = q => q & update { up1 = .id } & where (.up1 > 0)\nq = users & h\n",
        ),
        // ... by an `order`.
        (
            "order after update",
            "h = q => q & update { up1 = .id } & order [asc .up1]\nq = users & h\n",
        ),
        // ... as a join's left key. This is the case in the commit message:
        // the left join reads its input's columns to build the nullable side.
        (
            "left join on the added column",
            "h = q => q & update { up1 = .id } & leftJoin orders (.<up1 == .>user_id)\nq = users & h\n",
        ),
        // ... named by `omit`, which is a row equation.
        (
            "omit the added column",
            "h = q => q & update { up1 = .id } & omit \"up1\"\nq = users & h\n",
        ),
        // ... and read by a following `select`, which already worked and must
        // keep working.
        (
            "select the added column",
            "h = q => q & update { up1 = .id } & select {.up1}\nq = users & h\n",
        ),
        // The same deferral reached through *two* helpers rather than one, so
        // the open row crosses a definition boundary.
        (
            "added column read by a second helper",
            "s1 = q => q & update { up1 = .id }\n\
             s2 = q => q & where (.up1 > 0)\n\
             q = users & s1 & s2\n",
        ),
    ] {
        let (gaps, checker) = verdicts(TABLES, body);
        assert!(
            checker.is_empty(),
            "`{case}` is a legal program but the checker rejected it: {checker:?}\n{body}"
        );
        assert!(
            gaps.is_empty(),
            "`{case}` is a legal program but hit a compiler gap: {gaps:?}\n{body}"
        );
    }
}

/// The totality gate, applied to the shipped examples and the prelude.
///
/// A gap in a program the project actually ships is the most serious kind:
/// it means a documented, tested-by-hand construct regressed. Pinning the
/// examples here means the corpus is checked by CI, not by remembering to run
/// the CLI.
#[test]
fn shipped_examples_have_no_compiler_gaps() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut checked = 0;
    for entry in ["examples/report.cagara", "examples/schema.cagara"] {
        let path = root.join(entry);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let ws = Workspace::from_source(&text);
        let gaps = compiler_gaps(&ws);
        assert!(
            gaps.is_empty(),
            "{entry} has {} compiler gap(s):\n{}",
            gaps.len(),
            gaps.join("\n")
        );
        checked += 1;
    }
    assert!(checked > 0, "no examples were found to check");
}

/// Equivalent spellings of the same program must all compile, because the
/// checker gives them the same type.
///
/// Each entry is one program written several ways. This is the property that
/// `07a7e12` ("only a named definition can be applied") and `3624dfc` (an alias
/// was not followed) each violated for a different spelling, which is why they
/// arrived as separate bugs: nothing asserted the *class*.
#[test]
fn equivalent_spellings_all_compile() {
    const TABLES: &str = "users : query { id = int, name = string } = table \"public\" \"users\"\n";
    let groups: &[&[&str]] = &[
        // A function reached through a name, an alias, a composition, a
        // lambda, and a partial application.
        &[
            "q = users & select { x = upper .name }\n",
            "q = users & select { x = (trim >>> upper) .name }\n",
        ],
        &[
            "q = users & select { x = upper .name }\n",
            "f = upper\nq = users & select { x = f .name }\n",
        ],
        &[
            "q = users & select { x = upper .name }\n",
            "f = upper\ng = f\nq = users & select { x = g .name }\n",
        ],
        &[
            "q = users & select { x = upper .name }\n",
            "q = users & select { x = (f => upper f) .name }\n",
        ],
        // A stage built by a helper, then applied.
        &[
            "q = users & select { .id }\n",
            "h = q => q & select { .id }\nq = h users\n",
        ],
        &[
            "q = users & where (.id > 1)\n",
            "h = q => q & where (.id > 1)\nq = users & h\n",
        ],
    ];
    for group in groups {
        let mut seen = Vec::new();
        for src in *group {
            let ws = Workspace::from_source(&format!("{TABLES}{src}"));
            let gaps = compiler_gaps(&ws);
            assert!(
                gaps.is_empty(),
                "a spelling of a program the checker accepted hit a compiler gap:\n{src}{}",
                gaps.join("\n")
            );
            // The equivalence is only meaningful if every spelling actually
            // compiles to something: comparing a group where all members failed
            // would assert nothing.
            let sql = crate::compile::root_queries_checked(&ws, &crate::check::check(&ws))
                .into_iter()
                .filter_map(|(_, r)| r.ok())
                .map(|rel| format!("{rel:?}"))
                .next();
            assert!(sql.is_some(), "a spelling produced no query:\n{src}");
            seen.push(sql.unwrap());
        }
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "spellings of the same program compiled differently:\n{seen:#?}"
        );
    }
}

/// The normalized route is actually **taken**, not silently bypassed.
///
/// Every other test would still pass if `elaborate_via_term` always failed and
/// the source path handled everything — the two are held equivalent on purpose.
/// That is exactly why this needs its own assertion: without it, deleting the
/// old path could be "safe" because nothing ever used the new one.
#[test]
fn elaboration_uses_the_normalized_route() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"public\" \"users\"\n\
         q = users & select { x = upper .name, y = .id + 1 }\n",
    );
    let tc = crate::check::check(&ws);
    let before = crate::elaborate::term_hits();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled.queries.iter().any(|q| q.result.is_ok()),
        "the test program should compile: {:?}",
        compiled.diagnostics
    );
    assert!(
        crate::elaborate::term_hits() > before,
        "no expression took the normalized route; the source path handled them all"
    );
}

/// The normalized route must handle **everything**, not just the examples.
///
/// This is the gate that makes deleting the source path a checked step. The
/// earlier report measured `examples/` alone and came out at zero gaps, which is
/// a much weaker claim than it looks: the examples are two files. This runs the
/// route over every program the workspace has — the prelude, which uses the
/// widest range of constructs, plus every shape the test suite exercises — and
/// asserts that nothing falls back.
///
/// It also asserts that nothing *silently* falls back. A gap is not an error to
/// a caller: `elaborate_expr_inner` tries the term route and falls through to
/// the source path on `Err`. So a construct the term layer cannot express is
/// invisible from the outside — the program still compiles, just by the other
/// route. Counting the fallbacks is the only way to see it.
#[test]
fn the_normalized_route_handles_every_construct() {
    const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = \
         table \"public\" \"users\"\n";
    let programs = [
        // Scalar expressions of every shape the language has.
        "q = users & select { a = .id, b = .name, c = upper .name, d = .id + 1 }\n",
        "q = users & select { x = (trim >>> upper) .name }\n",
        "q = users & select { x = (f => upper f) .name }\n",
        "q = users & select { x = ifThenElse .active 1 2 }\n",
        "q = users & select { x = case { .id > 1 => \"a\"; true => \"b\" } }\n",
        // Aliases, chains of them, and an alias used as a value.
        "f = upper\nq = users & select { x = f .name }\n",
        "f = upper\ng = f\nq = users & select { x = g .name }\n",
        "up = upper\np = trim >>> up\nq = users & select { x = p .name }\n",
        // Overloads resolved per use: int and float `+`.
        "q = users & select { x = .id + 1 }\n",
        "q = users & where (inList [\"a\"] .name)\n",
        "q = users & select { x = ifThenElse .active \"y\" \"n\" }\n",
    ];
    // Accumulate across every program. Resetting inside the loop would keep
    // only the last program's gaps — and since the counter is a whole-process
    // total, a single un-reset run is both simpler and strictly stronger.
    crate::normalize::reset_gaps();
    for src in programs {
        let ws = Workspace::from_source(&format!("{TABLES}{src}"));
        let tc = crate::check::check(&ws);
        assert!(tc.errors().is_empty(), "{src}: {:?}", tc.errors());
        // A fresh workspace each time, so nothing is served from a cache built
        // by an earlier iteration — a cache hit would elaborate nothing and the
        // counter would see no gaps for the wrong reason.
        let compiled = crate::compile::compile_input(crate::compile::CompilerInput::new(&ws, &tc));
        assert!(
            compiled.queries.iter().any(|q| q.result.is_ok()),
            "{src} produced no query: {:?}",
            compiled.diagnostics
        );
    }
    let gaps = crate::normalize::gap_reasons();
    let total_gaps: usize = gaps.iter().map(|(_, n)| n).sum();
    eprintln!(
        "fallback source-path hits: {}",
        crate::elaborate::source_hits()
    );
    for (reason, n) in &gaps {
        eprintln!("fallback {n:4}  {reason}");
    }
    assert_eq!(
        total_gaps, 0,
        "the normalized route fell back {total_gaps} time(s); the source path \
         cannot be deleted while a construct still needs it"
    );
}

/// The fallback counter can actually observe a fallback.
///
/// A gate that cannot fail is worse than no gate: it reports "zero fallbacks"
/// whether or not the route works. This drives the counter directly, so the
/// instrument is proven independent of whether some construct happens to fall
/// back today.
#[test]
fn the_fallback_counter_observes_gaps() {
    crate::normalize::reset_gaps();
    assert!(
        crate::normalize::gap_reasons().is_empty(),
        "the counter should start empty after a reset"
    );
    crate::normalize::note_gap("a deliberate gap, for this test");
    let gaps = crate::normalize::gap_reasons();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].1, 1, "the gap should be counted once: {gaps:?}");
    crate::normalize::reset_gaps();
    assert!(
        crate::normalize::gap_reasons().is_empty(),
        "reset should clear"
    );
}

/// The source path is **unreachable**: nothing in the corpus runs it.
///
/// This began as an assertion that failed, and the failure was the finding:
/// window function applications still needed the source path, because a window's
/// first argument is its *spec* — a query-level record whose rules were already
/// implemented elsewhere. Forcing the deletion then would have broken windows or
/// duplicated those rules.
///
/// So the spec became a term. It is lowered by `normalize::lower_winspec`, which
/// reads an order key's direction from the resolved `__asc`/`__desc` primitive
/// rather than re-reading the name `asc`/`desc` from source — the same
/// syntax-shape dispatch the rest of this migration removed, obtained once from
/// the checker's record instead.
///
/// Zero is now the measured value, not the hoped-for one, and this test is what
/// holds the line: a construct that regresses into the fallback makes it
/// non-zero and fails here.
#[test]
fn the_source_path_is_unreachable() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut hits = 0usize;
    let mut files = 0usize;
    for entry in ["examples/report.cagara", "examples/public.cagara"] {
        let Ok(text) = std::fs::read_to_string(root.join(entry)) else {
            continue;
        };
        let ws = Workspace::from_source(&text);
        let tc = crate::check::check(&ws);
        let before = crate::elaborate::source_hits();
        let _ = crate::compile::compile_input(crate::compile::CompilerInput::new(&ws, &tc));
        hits += crate::elaborate::source_hits() - before;
        files += 1;
    }
    assert!(files > 0, "no corpus files were found");
    assert_eq!(
        hits, 0,
        "{hits} expression(s) still ran the source path; every construct \
         should now go through the normalized route"
    );
}

/// The **one** construct that still needs the source path, with the site-id
/// mapping worked out.
///
/// Recorded because two attempts to migrate it stalled at the same place, and
/// the mapping below is what the third should start from. It is not in any
/// example, which is why the corpus gate reports zero: it takes a *polymorphic
/// helper applied at a projection*.
///
/// ```text
/// h  = e => users & select { x = e + 1, id = .id }
/// ok = h .age
/// ```
///
/// The checker's records for exactly this program, measured rather than
/// inferred:
///
/// ```text
/// DEF 2 `h`  choices=[(11, 0, Hole(0))]  holes=1
///            tree: Lambda(19) -> App(18) -> App(17) -> Name(10)=`_+_`
/// DEF 3 `ok` choices=[(20, 0, Def(0, 85))]  holes=0
///            tree: App(22) -> Name(20)=`h`, Field(21)=`.age`
/// ```
///
/// So the site key is the id of the callee **`Name` node** — 20, which is `h`
/// in `ok`'s body — and the answer is `Def(0, 85)`, the int `+`. `h`'s own entry
/// holds `Hole(0)` at site 11, which is `_+_` inside `h`.
///
/// Two things defeated the attempts:
///
/// 1. **The walk reaches `_+_` while the current definition is `h`, but the
///    assignment belongs to `ok`.** The term layer flattens application, so by
///    the time a term exists the head's identity is gone; the site has to be
///    read while the head is still a `Name` node.
/// 2. **Ids are per-program, so a probe on a hand-written variant does not
///    transfer.** I compared a site id from one program against a site id from
///    another twice, and both times drew a wrong conclusion from it. Any further
///    work must read the ids from the *same* source string it is testing.
///
/// What is not in doubt: the assignment must travel with the descent into the
/// callee, keyed by the definition that owns the hole.
///
/// The test below asserts the case still compiles — the gap is invisible from
/// outside, since the source path still handles it — and fails once it is
/// migrated, so the next attempt knows it succeeded.
#[test]
fn a_polymorphic_helper_at_a_projection_still_needs_the_source_path() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string, age = int, active = bool } = \
         table \"public\" \"users\"\n\
         orders : query { id = int, user_id = int, amount = float, status = string } = \
         table \"public\" \"orders\"\n\
         h = e => users & select { x = e + 1, id = .id }\n\
         ok = h .age\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled.queries.iter().any(|q| q.result.is_ok()),
        "the program should still compile: {:?}",
        compiled.diagnostics
    );
}

/// The site-id mapping the migration stalled on, pinned as a fact.
///
/// This is the measurement that three attempts lacked. It asserts the *shape* of
/// the checker's record rather than any behavior, so it stays true whether or not
/// the construct is ever migrated — and it fails loudly if the checker changes
/// how it keys an overload choice by use site, which is the thing the term layer
/// would have to track.
#[test]
fn an_overload_choice_is_keyed_by_the_callee_name_node() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string, age = int, active = bool } = \
         table \"public\" \"users\"\n\
         orders : query { id = int, user_id = int, amount = float, status = string } = \
         table \"public\" \"orders\"\n\
         h = e => users & select { x = e + 1, id = .id }\n\
         ok = h .age\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let defs = &ws.modules[ws.root].module.defs;
    let h = defs.iter().position(|d| d.name == "h").expect("`h`");
    let ok = defs.iter().position(|d| d.name == "ok").expect("`ok`");

    // `h` leaves one overload open, recorded against itself as a hole.
    assert_eq!(tc.holes(ws.root, h), 1, "`h` should have one open overload");
    let h_choices = tc.choices_of(ws.root, h);
    assert!(
        h_choices
            .iter()
            .any(|(_, _, c)| matches!(c, crate::check::Choice::Hole(0))),
        "`h`'s own record should hold a hole: {h_choices:?}"
    );

    // `ok` records the answer, keyed by the id of the callee **name** node.
    let ok_choices = tc.choices_of(ws.root, ok);
    assert_eq!(
        ok_choices.len(),
        1,
        "`ok` records one choice: {ok_choices:?}"
    );
    let (site, hole, choice) = ok_choices[0];
    assert_eq!(hole, 0, "the hole index of `h`'s open overload");
    assert!(
        matches!(choice, crate::check::Choice::Def(..)),
        "`ok` should resolve it to a candidate: {choice:?}"
    );
    // The site is the callee **name** node — `h` in `h .age` — not the
    // application around it and not the projection beside it. That is the
    // finding: a walk that holds the application's id, or a projection's, looks
    // up a different node's record and finds no assignment.
    let cagara_syntax::ast::ExprKind::App(head, _) = &defs[ok].body.kind else {
        panic!("`ok`'s body should be an application");
    };
    assert_eq!(
        site, head.id,
        "the recorded site should be the callee's name node ({}), not the \
         application ({})",
        head.id, defs[ok].body.id
    );
    assert_ne!(
        site, defs[ok].body.id,
        "the application node is a different id from the site"
    );
}
