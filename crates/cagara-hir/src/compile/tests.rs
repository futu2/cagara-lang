use super::*;

fn assert_matches_fresh(ws: &Workspace) -> Compilation {
    let cached = compile(ws);
    let fresh = compile_checked(ws, &crate::check::check(ws));
    assert_eq!(cached, fresh);
    cached
}

/// The columns a stage node recorded, or none if the node's row is open.
fn node_columns(fs: &Option<Vec<(String, crate::core::ScalarType)>>) -> Vec<String> {
    fs.as_ref()
        .map(|fs| fs.iter().map(|(n, _)| n.clone()).collect())
        .unwrap_or_default()
}

/// The `(input columns, output columns)` of every row-rewriting stage in a
/// compiled query, innermost first — the order the checker recorded them.
///
/// The stages that only *keep* their input row pass it through; the ones that
/// build a row from expressions (`select`, `agg`, `update`, the joins) are not
/// rewrites and are skipped.
fn stage_rows(rel: &Rel) -> Vec<(Vec<String>, Vec<String>)> {
    fn walk(rel: &Rel, out: &mut Vec<(Vec<String>, Vec<String>)>) -> Vec<String> {
        let (input, output) = match rel {
            Rel::Table { columns, .. } => return columns.clone().unwrap_or_default(),
            // `At` stamps a location on each node and is transparent here, as
            // it is for the schema and lowering passes.
            Rel::At(_, inner) => return walk(inner, out),
            Rel::Where(inner, _)
            | Rel::Order(inner, _)
            | Rel::Limit(inner, _)
            | Rel::Offset(inner, _)
            | Rel::Distinct(inner) => return walk(inner, out),
            Rel::Prefix(inner, affix) => {
                let cols = walk(inner, out);
                let renamed = crate::schema::rename_columns(&cols, affix, true);
                (cols, renamed)
            }
            Rel::Suffix(inner, affix) => {
                let cols = walk(inner, out);
                let renamed = crate::schema::rename_columns(&cols, affix, false);
                (cols, renamed)
            }
            Rel::Omit(inner, key) => {
                let cols = walk(inner, out);
                let kept = crate::schema::omit_columns(&cols, key).expect("a checked omit");
                (cols, kept)
            }
            // Not a row rewrite, and not comparable here.
            _ => return Vec::new(),
        };
        out.push((input, output.clone()));
        output
    }
    let mut out = Vec::new();
    walk(rel, &mut out);
    out
}

#[test]
fn stage_nodes_agree_with_the_relational_rows() {
    // The checker records what each stage read and produced; the relational tree
    // carries the rows the schema layer derives from the *same* operation. The
    // two must agree — a disagreement is exactly the class of fault this
    // redesign exists to remove: two answers to one question, drifting.
    let src = "users : query { id = int, name = string } = table \"public\" \"users\"\n\
               q = users & prefix \"u_\" & suffix \"_v2\"\n\
               r = users & omit \"name\" & prefix \"x_\"\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    let compilation = compile_checked(&ws, &tc);
    assert!(
        compilation.diagnostics.is_empty(),
        "{:?}",
        compilation.diagnostics
    );

    let nodes: Vec<_> = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.name == "q" || d.name == "r")
        .map(|(i, d)| {
            let rel = compilation
                .queries
                .iter()
                .find(|query| query.id.def == i)
                .and_then(|query| query.result.as_ref().ok())
                .expect("both definitions compile");
            let rel = match rel {
                Rel::At(_, inner) => inner,
                other => other,
            };
            let compiled = stage_rows(rel);
            let recorded: Vec<(Vec<String>, Vec<String>)> = tc
                .row_stages(ws.root, i)
                .filter(|s| {
                    matches!(
                        s.kind,
                        crate::check::RowStage::Prefix
                            | crate::check::RowStage::Suffix
                            | crate::check::RowStage::Omit
                            | crate::check::RowStage::MapValue
                    )
                })
                .map(|s| (node_columns(&s.input), node_columns(&s.output)))
                .collect();
            (d.name.clone(), recorded, compiled)
        })
        .collect();

    for (name, recorded, compiled) in nodes {
        assert_eq!(
            recorded, compiled,
            "`{name}`: the checker's stage rows and the relational rows disagree"
        );
    }
}

#[test]
fn production_stage_check_handles_repeated_helpers_and_imports() {
    // Repeated stages exercise the one-record-one-match rule. The helper and
    // imported query exercise the ownership boundary: their stage records live
    // with the definition that applies them, not with the caller that inlines
    // them.
    let root_path = std::path::PathBuf::from("/tmp/cagara-stage-check/main.cagara");
    let lib_path = std::path::PathBuf::from("/tmp/cagara-stage-check/lib.cagara");
    let root = "import \"lib.cagara\" as lib\n\
               users : query { id = int } = table \"s\" \"users\"\n\
               helper = q => q & prefix \"h_\"\n\
               repeated = users & prefix \"a_\" & prefix \"b_\"\n\
               via_helper = users & helper\n\
               via_import = lib.query\n";
    let lib = "users : query { id = int } = table \"s\" \"lib_users\"\n\
               query = users & prefix \"l_\"\n";
    let buffers = HashMap::from([(lib_path, lib.to_string())]);
    let ws = Workspace::open_with_buffers(&root_path, root.into(), &buffers);
    let tc = crate::check::check(&ws);
    let compilation = compile_checked(&ws, &tc);
    assert!(
        compilation.diagnostics.is_empty(),
        "stage check rejected a valid helper/import program: {:?}",
        compilation.diagnostics
    );
}

/// A stage's identity is the **node that applied it**, and it survives inlining.
///
/// This is the property the stage check now rests on. The check used to compare
/// spans: it asked whether a record's node span lay inside the lowered stage's
/// span. That works by accident on the programs measured, and the accidents are
/// exactly what this test removes:
///
/// * **A helper's stage keeps the helper's identity.** `helper = q => q & prefix
///   "h_"` records a stage inside `helper`'s own body. When `helper` is inlined
///   into `via_helper`, the reduced term still carries that identity, naming
///   `helper` and the application node inside it — not the caller and not the
///   caller's span. So the caller's check does not claim the helper's stage, and
///   the helper's own definition is where it is checked.
/// * **Two stages in one pipeline are two identities**, even though the second
///   application's span encloses the first's — the case an interval test cannot
///   separate by containment alone.
#[test]
fn a_stage_identity_names_the_node_that_applied_it() {
    let src = "users : query { id = int } = table \"s\" \"users\"\n\
               helper = q => q & prefix \"h_\"\n\
               repeated = users & prefix \"a_\" & prefix \"b_\"\n\
               via_helper = users & helper\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let index = |name: &str| {
        ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("no definition `{name}`"))
    };

    // Two adjacent stages in one definition are two records, in application
    // order, with distinct identities.
    let repeated: Vec<_> = tc.row_stages(ws.root, index("repeated")).collect();
    assert_eq!(repeated.len(), 2, "two stages: {repeated:?}");
    assert_eq!(repeated[0].kind, crate::check::RowStage::Prefix);
    assert_ne!(
        repeated[0].node, repeated[1].node,
        "two applications are two nodes, not one"
    );
    assert_eq!(
        repeated[0].node.definition,
        index("repeated"),
        "each is attributed to the definition that applied it"
    );

    // The helper's own stage is recorded against `helper`, and the caller that
    // inlines it records no stage of its own — a helper's use is not a stage
    // application.
    let helper_stages: Vec<_> = tc.row_stages(ws.root, index("helper")).collect();
    assert_eq!(helper_stages.len(), 1, "the helper applies one stage");
    assert_eq!(helper_stages[0].node.definition, index("helper"));
    let caller_stages: Vec<_> = tc.row_stages(ws.root, index("via_helper")).collect();
    assert!(
        caller_stages.is_empty(),
        "a helper's use records nothing against the caller: {caller_stages:?}"
    );

    // And the compiler accepts the program, which is where the ownership
    // boundary is actually exercised: the caller's lowering carries the
    // helper's identity, and the check does not mistake it for the caller's.
    let compilation = compile_checked(&ws, &tc);
    assert!(
        compilation.diagnostics.is_empty(),
        "the stage check rejected a valid helper program: {:?}",
        compilation.diagnostics
    );
}

/// When normalization runs, the bridge visits every typed node except a
/// projection's base.
///
/// Normalization walks source syntax, and the checker's tree is its own account
/// of the same body. For query shapes that still use normalization, the two
/// have to cover the same nodes before a phase can take children from the tree
/// rather than re-deriving them from syntax.
///
/// The one deliberate difference is `base.field`. The checker records what the
/// *projection* resolved to — `infer_proj` writes the field's binding at the
/// projection node's own `ExprId` — so `base` is syntax whose meaning the
/// projection already carries, and the bridge resolves the projection without
/// stepping into it. The tree still holds the base, because the tree covers the
/// AST exactly. The test states that difference rather than tolerating it: the
/// nodes the bridge did not visit must be exactly the subtrees rooted at a
/// projection's base, so a second difference cannot hide behind the first.
///
/// Compared per definition, because the bridge moves to a callee's module and
/// definition when it inlines one: the visit log carries where each node was
/// written, so a definition's visits can be compared with its own tree.
#[test]
fn normalized_bridge_visits_every_typed_node_except_projection_bases() {
    let mut files = Vec::new();
    for dir in [
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples"),
    ] {
        collect_cagara(&dir, &mut files);
    }
    files.sort();
    assert!(!files.is_empty(), "no shipped programs were found");

    let mut compared = 0usize;
    let mut mismatched = Vec::new();
    for path in files {
        let ws = Workspace::open(&path);
        let tc = crate::check::check(&ws);
        crate::normalize::clear_visited();
        let _compilation = compile_checked(&ws, &tc);
        let mut visited: std::collections::HashMap<
            (usize, usize),
            std::collections::BTreeSet<u32>,
        > = Default::default();
        for (module, def, id) in crate::normalize::visited_nodes() {
            visited.entry((module, def)).or_default().insert(id);
        }
        for (&(module, def), ids) in &visited {
            let Some(tree) = tc.typed_body(module, def) else {
                continue;
            };
            // What the walk skips by design: every node under a projection's
            // base, which is the one place syntax is not a semantic step.
            let mut skipped = std::collections::BTreeSet::new();
            projection_bases(&ws.modules[module].module.defs[def].body, &mut skipped);
            let mut expected: Vec<u32> = tree
                .nodes()
                .iter()
                .map(|n| n.id)
                .filter(|id| !skipped.contains(id))
                .collect();
            expected.sort_unstable();
            let visited_ids: Vec<u32> = ids.iter().copied().collect();
            if visited_ids != expected {
                let text = &ws.modules[module].text;
                let describe = |ids: &[u32]| -> String {
                    ids.iter()
                        .map(|id| {
                            let node = tree.node(*id).expect("an id from the tree");
                            let src = text
                                .get(node.span.start as usize..node.span.end as usize)
                                .unwrap_or("<out of range>");
                            format!("{id}@{:?}={src:?}", node.span)
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                mismatched.push(format!(
                    "{} / module {module} def {def}:\n  visited:  {}\n  expected: {}",
                    path.display(),
                    describe(&visited_ids),
                    describe(&expected)
                ));
            }
            compared += 1;
        }
    }
    assert!(
        mismatched.is_empty(),
        "the bridge and the typed tree disagree about which nodes a body has:\n{}",
        mismatched.join("\n")
    );
    assert!(
        compared > 0,
        "no shipped program had both a bridge walk and a typed tree"
    );
}

#[test]
fn direct_typed_pipeline_bypasses_normalized_elaboration() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"public\" \"users\"\n\
         renamed = users & prefix \"u_\"\n\
         q = renamed & limit 10 & offset 2 & distinct\n\
             & suffix \"_v2\" & omit \"u_name_v2\" & mapValue \"nullable\"\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    crate::normalize::clear_visited();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled
            .queries
            .iter()
            .any(|query| query.name == "q" && query.result.is_ok()),
        "the typed pipeline failed to compile: {:?}",
        compiled.diagnostics
    );
    let q = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|def| def.name == "q")
        .unwrap();
    assert!(
        !crate::normalize::visited_nodes()
            .iter()
            .any(|(module, def, _)| *module == ws.root && *def == q),
        "direct typed pipeline unexpectedly walked normalized terms"
    );
}

#[test]
fn direct_plain_select_bypasses_normalized_elaboration() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"public\" \"users\"\n\
         q = users & select { id = .id, name = .name, tag = \"active\" }\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    crate::normalize::clear_visited();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled
            .queries
            .iter()
            .any(|query| query.name == "q" && query.result.is_ok()),
        "the typed projection failed to compile: {:?}",
        compiled.diagnostics
    );
    let q = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|def| def.name == "q")
        .unwrap();
    assert!(
        !crate::normalize::visited_nodes()
            .iter()
            .any(|(module, def, _)| *module == ws.root && *def == q),
        "plain direct select unexpectedly walked normalized terms"
    );
}

#[test]
fn direct_plain_update_bypasses_normalized_elaboration() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"public\" \"users\"\n\
         q = users & update { name = .name, tag = \"active\" }\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    crate::normalize::clear_visited();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled
            .queries
            .iter()
            .any(|query| query.name == "q" && query.result.is_ok()),
        "the typed update failed to compile: {:?}",
        compiled.diagnostics
    );
    let q = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|def| def.name == "q")
        .unwrap();
    assert!(
        !crate::normalize::visited_nodes()
            .iter()
            .any(|(module, def, _)| *module == ws.root && *def == q),
        "plain direct update unexpectedly walked normalized terms"
    );
}

#[test]
fn direct_plain_order_bypasses_normalized_elaboration() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string } = table \"public\" \"users\"\nq = users & order [desc .name]\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    crate::normalize::clear_visited();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled
            .queries
            .iter()
            .any(|query| query.name == "q" && query.result.is_ok()),
        "the typed order failed to compile: {:?}",
        compiled.diagnostics
    );
    let q = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|def| def.name == "q")
        .unwrap();
    assert!(
        !crate::normalize::visited_nodes()
            .iter()
            .any(|(module, def, _)| *module == ws.root && *def == q),
        "plain direct order unexpectedly walked normalized terms"
    );
}

#[test]
fn direct_plain_where_bypasses_normalized_elaboration() {
    let ws = Workspace::from_source(
        "users : query { id = int, age = int, name = string } = table \"public\" \"users\"\nq = users & where (in [18, 21] .age && .name != \"guest\")\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    crate::normalize::clear_visited();
    let compiled = compile_checked(&ws, &tc);
    assert!(
        compiled
            .queries
            .iter()
            .any(|query| query.name == "q" && query.result.is_ok()),
        "the typed where failed to compile: {:?}",
        compiled.diagnostics
    );
    let q = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|def| def.name == "q")
        .unwrap();
    assert!(
        !crate::normalize::visited_nodes()
            .iter()
            .any(|(module, def, _)| *module == ws.root && *def == q),
        "plain direct where unexpectedly walked normalized terms"
    );
}

/// Every node under a projection's base, for the bridge/tree agreement test.
///
/// The whole base subtree is skipped, not only its root: `a.b.c` is a projection
/// of a projection, and the bridge resolves the outer field without walking the
/// inner one either.
fn projection_bases(e: &cagara_syntax::ast::Expr, out: &mut std::collections::BTreeSet<u32>) {
    if let cagara_syntax::ast::ExprKind::Proj(base, _) = &e.kind {
        let mut subtree = vec![base.as_ref()];
        while let Some(e) = subtree.pop() {
            out.insert(e.id);
            subtree.extend(crate::check::child_exprs(e));
        }
    }
    for child in crate::check::child_exprs(e) {
        projection_bases(child, out);
    }
}

/// The same agreement, over every program the repository ships.
///
/// The unit test above writes down the shapes a rename chain and an `omit`
/// take. The corpus is where a program that reaches a stage another way would
/// show up — through an alias, a helper, a partial application, an import —
/// which is exactly how the reported defect travelled.
#[test]
fn shipped_programs_stage_nodes_agree_with_the_relational_rows() {
    let mut files = Vec::new();
    for dir in [
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples"),
    ] {
        collect_cagara(&dir, &mut files);
    }
    files.sort();
    assert!(!files.is_empty(), "no shipped programs were found");

    let mut compared = 0usize;
    for path in files {
        // `open` rather than `from_source`: a corpus case may import a sibling.
        let ws = Workspace::open(&path);
        let tc = crate::check::check(&ws);
        let compilation = compile_checked(&ws, &tc);
        for (i, def) in ws.modules[ws.root].module.defs.iter().enumerate() {
            let recorded: Vec<(Vec<String>, Vec<String>)> = tc
                .row_stages(ws.root, i)
                .filter(|s| {
                    matches!(
                        s.kind,
                        crate::check::RowStage::Prefix
                            | crate::check::RowStage::Suffix
                            | crate::check::RowStage::Omit
                            | crate::check::RowStage::MapValue
                    )
                })
                .map(|s| (node_columns(&s.input), node_columns(&s.output)))
                .collect();
            if recorded.is_empty() {
                continue;
            }
            // A definition that failed has no relational rows to compare with;
            // the corpus deliberately holds programs that do not check.
            let Some(Ok(rel)) = compilation
                .queries
                .iter()
                .find(|query| query.id.def == i)
                .map(|query| &query.result)
            else {
                continue;
            };
            let rel = match rel {
                Rel::At(_, inner) => inner,
                other => other,
            };
            assert_eq!(
                recorded,
                stage_rows(rel),
                "{} / `{}`: the checker's stage rows and the relational rows disagree",
                path.display(),
                def.name
            );
            compared += 1;
        }
    }
    assert!(
        compared > 0,
        "no shipped program applied a row-rewriting stage"
    );
}

/// Every `.cagara` file under `dir`, recursively.
fn collect_cagara(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cagara(&path, out);
        } else if path.extension().is_some_and(|e| e == "cagara") {
            out.push(path);
        }
    }
}

/// The accessors answer what the typed nodes hold, over every shipped program.
///
/// The tree replaced the side maps, so this is the check that the boundary a
/// consumer reads through — `use_ty`, `use_scheme`, `use_type`, `resolution`,
/// `choice`, `choices_of`, `holes` — says what the nodes say. Every node of
/// every shipped program is compared: its scalar type against `use_ty`, its row
/// against `use_scheme`, its printed type against `use_type`, its resolution
/// against `resolution`, and its choices against `choice`.
///
/// The node *set* is compared too, against a walk of the AST using the same
/// child order the tree was built with — a tree that quietly dropped a node
/// would otherwise pass every per-node check. That walk is the one comparison
/// here with a source outside the tree.
#[test]
fn the_accessors_answer_what_the_typed_nodes_hold() {
    let mut files = Vec::new();
    for dir in [
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples"),
    ] {
        collect_cagara(&dir, &mut files);
    }
    files.sort();
    assert!(!files.is_empty(), "no shipped programs were found");

    let mut compared = 0usize;
    for path in files {
        let ws = Workspace::open(&path);
        let tc = crate::check::check(&ws);
        for (i, def) in ws.modules[ws.root].module.defs.iter().enumerate() {
            let Some(tree) = tc.typed_body(ws.root, i) else {
                continue;
            };
            // Every AST node is in the tree, and every tree node is in the AST.
            let mut ast_ids = Vec::new();
            let mut pending = vec![&def.body];
            while let Some(e) = pending.pop() {
                ast_ids.push(e.id);
                pending.extend(crate::check::child_exprs(e));
            }
            let mut tree_ids: Vec<u32> = tree.nodes().iter().map(|n| n.id).collect();
            ast_ids.sort_unstable();
            tree_ids.sort_unstable();
            assert_eq!(
                tree_ids,
                ast_ids,
                "{} / `{}`: the tree does not cover the body",
                path.display(),
                def.name
            );
            // The definition's open-overload count, and every choice, are the
            // tree's account of the definition rather than of one node.
            assert_eq!(
                tree.holes,
                tc.holes(ws.root, i),
                "{} / `{}`: open overloads",
                path.display(),
                def.name
            );
            let mut from_nodes: Vec<(u32, usize, crate::check::Choice)> = tree
                .nodes()
                .iter()
                .flat_map(|node| {
                    node.choices
                        .iter()
                        .map(move |(hole, choice)| (node.id, *hole, *choice))
                })
                .collect();
            from_nodes.sort_by_key(|&(site, hole, _)| (site, hole));
            assert_eq!(
                from_nodes,
                tc.choices_of(ws.root, i),
                "{} / `{}`: the choices the nodes hold are the definition's",
                path.display(),
                def.name
            );

            for node in tree.nodes() {
                // The choices recorded at this node's site, which is what
                // `choice` and `choices_of` read.
                for (hole, choice) in &node.choices {
                    assert_eq!(
                        tc.choice(ws.root, i, node.id, *hole),
                        Some(*choice),
                        "{} / `{}`: node {} hole {}",
                        path.display(),
                        def.name,
                        node.id,
                        hole
                    );
                }
                assert_eq!(
                    node.ty(),
                    tc.use_ty(ws.root, node.id),
                    "{} / `{}`: node {} scalar type",
                    path.display(),
                    def.name,
                    node.id
                );
                let mapped_row = match tc.use_scheme(ws.root, node.id) {
                    Some(crate::check::SchemeView::Query { fields }) => fields,
                    _ => None,
                };
                assert_eq!(
                    node.row(),
                    mapped_row,
                    "{} / `{}`: node {} row",
                    path.display(),
                    def.name,
                    node.id
                );
                // The printed type is keyed by *span* on the boundary, and the
                // span recorded for a use is not always the node's own — a
                // deferred literal records its argument's — so compare where
                // that map has an entry and otherwise assert the node carries
                // a printed form at all.
                match tc.use_type(ws.root, node.span) {
                    Some(shown) => assert_eq!(
                        node.shown,
                        shown,
                        "{} / `{}`: node {} printed type",
                        path.display(),
                        def.name,
                        node.id
                    ),
                    None => assert!(
                        node.shown.is_empty() == (node.ty().is_none() && node.row().is_none()),
                        "{} / `{}`: node {} has a type but no printed form",
                        path.display(),
                        def.name,
                        node.id
                    ),
                }
                assert_eq!(
                    node.resolved.as_ref(),
                    tc.resolution(ws.root, node.id),
                    "{} / `{}`: node {} resolution",
                    path.display(),
                    def.name,
                    node.id
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 0, "no shipped program produced a typed tree");
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

/// A dependency's *row* change dirties the definitions that use it.
///
/// `DefinitionFacts` records a use's scalar type, and a query-typed use is
/// `None` on both sides of an edit — so this definition's own facts compare
/// equal and cannot be the proof. What has to catch it is the walk over
/// `dependencies`: the edited definition's own facts differ (its scheme's
/// fields do), and every definition that names it is reached from there.
///
/// The edit is chosen so a stale reuse would be *wrong*, not merely
/// redundant: `q` renames every column, so a row whose column is now `b` has
/// to produce `p_b`, not `p_a`. It is also the same length, so the definitions
/// after it keep their spans and are not dirtied merely by moving.
#[test]
fn a_row_change_dirties_the_definitions_that_use_it() {
    let src = "users : query { a = int } = table \"s\" \"t\"\n\
               q = users & prefix \"p_\"\n\
               alone : query { a = int } = table \"s\" \"own\"\n";
    let mut ws = Workspace::from_source(src);
    assert!(compile(&ws).is_ok());
    let before = crate::elaborate::elaborated_defs().len();
    // Only `users`'s declaration: `alone` has the same text and must keep it.
    assert!(ws.set_source(ws.root, src.replacen("{ a = int }", "{ b = int }", 1)));
    let edited = compile(&ws);
    let rerun = &crate::elaborate::elaborated_defs()[before..];
    assert!(
        rerun.contains(&(ws.root, 1)),
        "`q` names the edited row and must not reuse a stale one; rerun {rerun:?}"
    );
    assert!(
        !rerun.contains(&(ws.root, 2)),
        "an independent definition must still be reused; rerun {rerun:?}"
    );
    assert_eq!(
        edited,
        compile_checked(&ws, &crate::check::check(&ws)),
        "the reused compilation must equal a fresh one"
    );
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

/// A definition that expands past the term budget is reported, not emitted.
///
/// Inlining is substitution, so a definition that mentions its argument twice
/// doubles it at every level. Eighteen levels of `dup` is about a quarter of a
/// million nodes from 350 bytes of source — eight megabytes of SQL, ten seconds
/// of work — and each further level doubles both. The budget refuses it as a
/// *program* fault, since the program's shape is what expands.
#[test]
fn a_definition_that_expands_past_the_budget_is_reported() {
    let mut expr = String::from("1");
    for _ in 0..20 {
        expr = format!("dup ({expr})");
    }
    let ws = Workspace::from_source(&format!(
        "users : query {{ id = int }} = table \"s\" \"users\"\n\
         dup : expr r int -> expr r int = x => ifThenElse true x x\n\
         q = users & select {{ y = {expr} }}\n"
    ));
    let diags: Vec<Diag> = compile(&ws)
        .queries
        .iter()
        .filter_map(|q| q.result.as_ref().err().cloned())
        .collect();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("expands to more than"),
        "{}",
        diags[0]
    );
}

/// An impossible window frame is reported, not lowered.
///
/// `rows (following 1) (preceding 1)` asks for a frame that starts after it
/// ends. The checked layer has a constructor that knows this
/// (`checked::frame`), but the normalizer built the IR struct directly and
/// skipped it, so the query reached SQL as
/// `ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING` — which the engines reject, at
/// run time, in the one place the type checker exists to prevent.
#[test]
fn an_impossible_window_frame_is_reported() {
    let ws = Workspace::from_source(
        "users : query { id = int, age = int } = table \"s\" \"users\"\n\
         q = users & select { s = sumOver { partition = [.id], order = [asc .age], \
             frame = rows (following 1) (preceding 1) } .age }\n",
    );
    let diags: Vec<Diag> = compile(&ws)
        .queries
        .iter()
        .filter_map(|q| q.result.as_ref().err().cloned())
        .collect();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("impossible window frame"),
        "{}",
        diags[0]
    );
}

/// An elaboration error points at the *user's* file, not the prelude.
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
    // The span must be the offending **argument**, not the stage that wanted it.
    // `limit` starts at column 25 of the source line and `(1 + 1)` at 31, so a
    // diagnostic blaming the stage would report the earlier column. A
    // differential run against the pre-migration compiler is what showed this
    // mattered: the message had drifted to the whole stage while still mentioning
    // an int, so the message assertion above passed and only the caret was wrong.
    //
    // `limit` begins at byte 40 and `(1 + 1)` at byte 46 of the line, so the two
    // are distinguishable by column alone and the assertion is meaningful
    // whichever base the renderer uses: a caret on the stage would be at or
    // before 41, one on the argument at or after 47.
    assert!(
        d.col >= 47,
        "the caret must be on the argument (byte 46+), not on `limit` (byte 40): {d}"
    );
    assert_eq!(d.width, 5, "the caret should underline `1 + 1`: {d}");
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

/// Every representative query shape compiles without a capability gap.
///
/// The programs are the shapes that exercise the whole vocabulary: stages, a
/// join, a set operation, an aggregate, a window, a named stage, a composed
/// stage, an inline lambda, a `case`, and a pipeline.
///
/// The signal is the compiler fault attached to each definition, so this stays
/// meaningful as production moves query shapes from the normalized route to
/// typed reconstruction.
#[test]
fn all_query_forms_compile_without_a_gap() {
    const PROGRAMS: &[(&str, &str)] = &[
        ("a filter", "q = users & where (.age > 1)\n"),
        ("a projection", "q = users & select { .name }\n"),
        (
            "a join",
            "q = users & innerJoin orders (.<id == .>user_id)\n",
        ),
        ("a set operation", "q = users & union admins\n"),
        (
            "an aggregate",
            "q = users & agg { a = group .age, n = count }\n",
        ),
        (
            "a window",
            "q = users & select { r = rowNumber {order = [asc .id]} }\n",
        ),
        ("a named stage", "s = select { .id }\nq = users & s\n"),
        (
            "a composed stage",
            "s = where (.age > 1) >>> select { .id }\nq = users & s\n",
        ),
        (
            "an inline lambda stage",
            "q = users & (x => where (.age > 1) x)\n",
        ),
        (
            "a case",
            "q = users & select { x = case { .age > 1 => \"a\"; true => \"b\" } }\n",
        ),
        (
            "a pipeline",
            "q = users & where (.age > 1) & select { .id } & order [asc .id] & limit 2\n",
        ),
    ];
    const TABLES: &str = "users : query { id = int, name = string, age = int } = \
                          table \"public\" \"users\"\n\
                          orders : query { id = int, user_id = int, amount = float } = \
                          table \"public\" \"orders\"\n\
                          admins : query { id = int, name = string, age = int } = \
                          table \"public\" \"admins\"\n";

    for (label, body) in PROGRAMS {
        let ws = Workspace::from_source(&format!("{TABLES}{body}"));
        let tc = crate::check::check(&ws);
        assert!(tc.errors().is_empty(), "{label}: {:?}", tc.errors());
        for (name, _, fault) in crate::compile::root_verdicts(&ws, &tc) {
            assert_ne!(
                fault,
                Fault::Compiler,
                "{label}: `{name}` hit a compiler capability gap"
            );
        }
        let compiled = compile_checked(&ws, &tc);
        assert!(
            compiled.queries.iter().any(|q| q.result.is_ok()),
            "{label} produced no query: {:?}",
            compiled.diagnostics
        );
    }
}

/// The normalized route must handle **everything**, not just the examples.
///
/// This is the gate that makes the normalized route a checked step: every
/// expression shape the language has, over legal programs, must produce a query
/// with no compiler capability gap. A construct the term layer could not express
/// would surface as `Fault::Compiler` on the definition, which is what this
/// asserts against — the signal the route raises itself, rather than a counter
/// something else had to remember to increment.
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
    for src in programs {
        let ws = Workspace::from_source(&format!("{TABLES}{src}"));
        let tc = crate::check::check(&ws);
        assert!(tc.errors().is_empty(), "{src}: {:?}", tc.errors());
        let gaps: Vec<String> = crate::compile::root_verdicts(&ws, &tc)
            .into_iter()
            .filter(|(_, _, fault)| *fault == Fault::Compiler)
            .map(|(name, result, _)| {
                format!(
                    "`{name}`: {}",
                    result.err().map(|d| d.message).unwrap_or_default()
                )
            })
            .collect();
        assert!(
            gaps.is_empty(),
            "the term layer could not express {} construct(s) in {src:?}; with no \
             fallback left, each of these is a compiler gap for a real program:\n  {}",
            gaps.len(),
            gaps.join("\n  ")
        );
    }
}

/// Every corpus program compiles through the normalized route.
///
/// The gate that made this possible is gone with the thing it measured: there is
/// no longer a source path to fall back to, so "unreachable" is no longer a
/// property that can fail. What remains worth asserting is that the corpus still
/// compiles at all — a construct the term layer could not express would now be a
/// hard compiler gap rather than a silent fallback, so this fails loudly.
#[test]
fn every_corpus_program_compiles_without_a_fallback() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut checked = 0usize;
    for entry in [
        "examples/report.cagara",
        "examples/public.cagara",
        "examples/schema.cagara",
        "examples/errors.cagara",
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(entry)) else {
            continue;
        };
        let ws = Workspace::from_source(&text);
        let tc = crate::check::check(&ws);
        let compiled = crate::compile::compile_input(crate::compile::CompilerInput::new(&ws, &tc));
        for query in &compiled.queries {
            assert_ne!(
                query.fault,
                Some(Fault::Compiler),
                "{entry} / `{}` hit a compiler gap now that no fallback exists",
                query.name
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no corpus files were found");
}

/// Every use site's recorded overload choices are contiguous from zero, and no
/// site mixes a resolved choice with an open one.
///
/// `normalize::InputFacts::holes_at` keeps a site's resolved choices, drops the
/// open ones, and hands them to a normalizer that reads them **positionally**:
/// `Normalizer::hole(k)` is `assignment.get(k)`, documented as
/// `holes[k] = (callee_module, chosen_def)`. Dropping an entry shifts every later
/// position, so that read means what it says only under the two conditions this
/// asserts.
///
/// Both were measured rather than assumed, because the shapes have different
/// correct answers. A site of *open* holes is answered by the enclosing
/// assignment on the stack, and the source path that used to do this read it as
/// `cx.holes.get(h)` — translating the callee's hole number `h` into the
/// caller's — while a dropped entry simply falls through to whatever sits at
/// that index. The two agree only while the numbers line up, which is the
/// contiguity this pins.
///
/// Measured over the widest corpus available — the prelude, which uses almost
/// every construct, plus every shipped example — and it holds: no site mixes,
/// and none is sparse. So this is not a bug report; it is the invariant the
/// adapter silently relies on, stated where a change to it would fail loudly
/// rather than appear as a wrong overload candidate.
#[test]
fn every_use_site_records_contiguous_choices() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // The prelude always loads, and it is the hardest of the two corpora.
    let mut workspaces = vec![("the prelude".to_string(), Workspace::from_source("q = 1\n"))];
    for entry in [
        "examples/report.cagara",
        "examples/public.cagara",
        "examples/schema.cagara",
        "examples/errors.cagara",
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(entry)) {
            workspaces.push((entry.to_string(), Workspace::from_source(&text)));
        }
    }

    let mut sites = 0usize;
    for (label, ws) in &workspaces {
        let tc = crate::check::check(ws);
        for m in 0..ws.modules.len() {
            for d in 0..ws.modules[m].module.defs.len() {
                let mut by_site: std::collections::BTreeMap<
                    u32,
                    Vec<(usize, crate::check::Choice)>,
                > = std::collections::BTreeMap::new();
                for (site, hole, choice) in tc.choices_of(m, d) {
                    by_site.entry(site).or_default().push((hole, choice));
                }
                for (site, entries) in by_site {
                    sites += 1;
                    let mut holes: Vec<usize> = entries.iter().map(|(h, _)| *h).collect();
                    holes.sort_unstable();
                    assert_eq!(
                        holes,
                        (0..entries.len()).collect::<Vec<_>>(),
                        "{label}: definition {d} of module {m} records holes \
                         {holes:?} at site {site}; a gap makes `holes_at`'s \
                         positional read address the wrong hole"
                    );
                    let open = entries
                        .iter()
                        .any(|(_, c)| matches!(c, crate::check::Choice::Hole(_)));
                    let resolved = entries
                        .iter()
                        .any(|(_, c)| matches!(c, crate::check::Choice::Def(..)));
                    assert!(
                        !(open && resolved),
                        "{label}: definition {d} of module {m} records both a \
                         resolved and an open choice at site {site}: {entries:?} \
                         — dropping the open one shifts every later hole, so the \
                         assignment `holes_at` returns no longer matches its \
                         documented indexing"
                    );
                }
            }
        }
    }
    assert!(
        sites > 0,
        "no use site with a recorded choice was found; the scan is not looking \
         where it thinks it is"
    );
}

/// Every use site carries **one** overload hole, so `Facts::choice` never has to
/// choose between entries.
///
/// `choice_at` returns `None` when a site has several entries, deliberately:
/// without the hole index there is no way to tell which one the caller meant, so
/// reporting the use as unresolved keeps a coin-flip from being recorded as a
/// decision. This measures whether that branch is reachable — it is not, in any
/// program available here, because the checker keys choices by `(ExprId, hole)`
/// and each overloaded *application* is its own `ExprId`.
///
/// The assertion is therefore the *documentation* of a defensive branch: if a
/// program is ever found where one site really does carry two holes, this fails
/// and names it, and the fix is to thread the hole index through the trait
/// rather than to loosen the check.
///
/// The corpus is the same one the contiguity test uses, plus the shapes that
/// were expected to produce several holes and did not: nested sums, a definition
/// applied to two overloaded arguments, and a record of two sums.
#[test]
fn every_use_site_carries_one_hole() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut workspaces = vec![("the prelude".to_string(), Workspace::from_source("q = 1\n"))];
    for entry in [
        "examples/report.cagara",
        "examples/public.cagara",
        "examples/schema.cagara",
        "examples/errors.cagara",
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(entry)) {
            workspaces.push((entry.to_string(), Workspace::from_source(&text)));
        }
    }
    for (label, src) in [
        ("nested sums", "q = 1 + 2 + 3 + 4\n"),
        (
            "a definition applied to two overloaded arguments",
            "f : expr r int -> expr r int -> expr r int = a => b => a + b\nq = f (1 + 2) (3 + 4)\n",
        ),
        ("a record of two sums", "q = { a = 1 + 2, b = 3 + 4 }\n"),
        (
            "two overloaded columns compared",
            "q = (1 + 2) == (3 + 4)\n",
        ),
    ] {
        workspaces.push((label.to_string(), Workspace::from_source(src)));
    }

    let mut sites = 0usize;
    for (label, ws) in &workspaces {
        let tc = crate::check::check(ws);
        for m in 0..ws.modules.len() {
            for d in 0..ws.modules[m].module.defs.len() {
                let mut by_site: std::collections::BTreeMap<u32, Vec<usize>> =
                    std::collections::BTreeMap::new();
                for (site, hole, _) in tc.choices_of(m, d) {
                    by_site.entry(site).or_default().push(hole);
                }
                for (site, holes) in by_site {
                    sites += 1;
                    assert_eq!(
                        holes.len(),
                        1,
                        "{label}: definition {d} of module {m} records {holes:?} at \
                         site {site}. `Facts::choice` returns `None` for such a \
                         site, so this use resolves as an unknown overload; thread \
                         the hole index through `Facts::choice` rather than \
                         loosening that check"
                    );
                }
            }
        }
    }
    assert!(
        sites > 0,
        "no use site with a recorded choice was found; the scan is not looking \
         where it thinks it is"
    );
}

/// Both `Facts` adapters answer the hole-assignment question identically.
///
/// The workspace adapter (used by tests) did not override `holes_at`, so it fell
/// back to the trait's default — no assignment, every hole open — while the
/// compiler's adapter read the checker's choices. The walk then differed between
/// the two on `h .age`: production found the assignment, tests did not and let
/// the fall-through stand in for it. Both happened to produce the same term here,
/// which is exactly why nothing caught it.
///
/// This asserts the two agree rather than that either is "right", so it fails if
/// either one drifts. It is the cheap half of merging the adapters; the other
/// half is that they now share `resolved_assignment`.
#[test]
fn both_facts_adapters_resolve_the_same_assignment() {
    use crate::compile::CompilerInput;
    use crate::normalize::{Facts, InputFacts, WorkspaceFacts};
    use cagara_syntax::ast::ExprKind;

    let ws = Workspace::from_source(
        "users : query { id = int, name = string, age = int, active = bool } = \
         table \"public\" \"users\"\n\
         h = e => users & select { x = e + 1, id = .id }\n\
         ok = h .age\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let ok = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|d| d.name == "ok")
        .expect("`ok`");
    let ExprKind::App(head, _) = &ws.modules[ws.root].module.defs[ok].body.kind else {
        panic!("`ok`'s body should be an application");
    };
    let site = head.id;

    let workspace_facts = WorkspaceFacts::new(&ws, &tc);
    let input = CompilerInput::new(&ws, &tc);
    let input_facts = InputFacts::new(input);

    // The assignment is not empty — otherwise "both agree" would hold trivially
    // on a pair of empty vectors, which is the shape of the bug being guarded.
    let from_workspace = workspace_facts.holes_at((ws.root, ok), site);
    assert!(
        !from_workspace.is_empty(),
        "`ok` resolved `h`'s open overload, so its assignment must not be empty"
    );
    assert_eq!(
        from_workspace,
        input_facts.holes_at((ws.root, ok), site),
        "the test adapter and the compiler adapter disagree about the \
         assignment at site {site}"
    );
}

/// A polymorphic helper applied at a projection resolves its open overload.
///
/// This was the hardest construct to get right during the rewrite and the one
/// the corpus does not cover, so it is kept as a regression test with the
/// reasoning intact. A polymorphic helper applied at a projection:
///
/// ```text
/// h  = e => users & select { x = e + 1, id = .id }
/// ok = h .age
/// ```
///
/// `h`'s `+` is an open overload; `ok` records the answer at the id of the
/// callee *name* node. Elaborating `ok` inlines `h`'s body, so the walk reaches
/// `_+_` while the current definition is `h` but the assignment belongs to
/// `ok`.
///
/// Three things had to be right at once, and each was wrong in turn:
///
/// 1. **The choice is recorded against the caller.** `infer.rs` writes
///    `self.active.last()`. Asking about the callee returns nothing.
/// 2. **The site is the callee name node's id**, not the application's and not a
///    projection's — and flattening an application discards that node, so it has
///    to be read while the head is still a `Name`.
/// 3. **`cx.holes` is positional.** `holes[k]` is the assignment for hole `k`,
///    each entry being `(callee_module, chosen_def)`. A version that searched by
///    the tuple's first element was comparing a *module* index against a *hole*
///    index, and found nothing for as long as no module happened to be numbered
///    like a hole.
#[test]
fn a_polymorphic_helper_at_a_projection_resolves_its_overload() {
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
        "the program should compile: {:?}",
        compiled.diagnostics
    );
    for query in &compiled.queries {
        assert_ne!(
            query.fault,
            Some(Fault::Compiler),
            "`{}` should not need a fallback any more",
            query.name
        );
    }
}

/// The checker records an overload choice against the **caller**, at the callee
/// *name* node's id. Pinned as a fact so it does not have to be rediscovered.
#[test]
fn an_overload_choice_is_keyed_by_the_use_site() {
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

    // The callee leaves one overload open, recorded against itself as a hole.
    assert_eq!(tc.holes(ws.root, h), 1, "`h` should have one open overload");
    assert!(
        tc.choices_of(ws.root, h)
            .iter()
            .any(|(_, _, c)| matches!(c, crate::check::Choice::Hole(0))),
        "`h`'s own record should hold a hole: {:?}",
        tc.choices_of(ws.root, h)
    );

    // The caller holds the answer — this is the fact: recorded against `ok`, not
    // against `h`. Asking about the callee returns nothing.
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
    // The site is the callee *name* node, `h` in `h .age`, not the application.
    let cagara_syntax::ast::ExprKind::App(head, _) = &defs[ok].body.kind else {
        panic!("`ok`'s body should be an application");
    };
    assert_eq!(
        site, head.id,
        "the recorded site should be the callee name node ({}) rather than the \
         application ({})",
        head.id, defs[ok].body.id
    );
    assert_ne!(
        site, defs[ok].body.id,
        "the application node carries a different id from the site"
    );
}

/// An inherited open overload is re-attributed to the definition that
/// inherits it.
///
/// An overload constraint is born at a site (`overload.rs`), becomes
/// `Origin::Hole` when it is stored in a scheme, and becomes a *site* of the
/// using definition again when that scheme is instantiated (`infer.rs`). So a
/// definition that applies another definition's still-open overload records
/// the hole against **its own** use site, and `holes` — the count that decides
/// whether a definition is elaborated at all — agrees with the holes its
/// `choices` record, which is what makes the count readable from either.
#[test]
fn an_inherited_hole_is_recorded_against_the_definition_that_inherits_it() {
    let ws = Workspace::from_source(
        "users : query { id = int, age = int } = table \"public\" \"users\"\n\
         h = e => users & select { x = e + 1, id = .id }\n\
         mid = x => h x\n",
    );
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    let defs = &ws.modules[ws.root].module.defs;
    let h = defs.iter().position(|d| d.name == "h").expect("`h`");
    let mid = defs.iter().position(|d| d.name == "mid").expect("`mid`");

    // `h` opens the overload at the `+` in its own body.
    assert_eq!(tc.holes(ws.root, h), 1, "`h` leaves one overload open");
    let h_holes: Vec<_> = tc
        .choices_of(ws.root, h)
        .into_iter()
        .filter(|(_, _, c)| matches!(c, crate::check::Choice::Hole(_)))
        .collect();
    assert_eq!(h_holes.len(), 1, "`h` records its own hole: {h_holes:?}");

    // `mid` inherits it, and the hole is attributed to `mid`'s use of `h`.
    assert_eq!(
        tc.holes(ws.root, mid),
        1,
        "`mid` inherits `h`'s open overload"
    );
    let mid_holes: Vec<_> = tc
        .choices_of(ws.root, mid)
        .into_iter()
        .filter(|(_, _, c)| matches!(c, crate::check::Choice::Hole(_)))
        .collect();
    assert_eq!(
        mid_holes.len(),
        1,
        "`mid` records the hole it inherited: {:?}",
        tc.choices_of(ws.root, mid)
    );

    // The site is the callee *name* node of `mid`'s own body — not `h`'s `+`.
    let cagara_syntax::ast::ExprKind::Lambda(_, body) = &defs[mid].body.kind else {
        panic!("`mid`'s body should be a lambda");
    };
    let cagara_syntax::ast::ExprKind::App(head, _) = &body.kind else {
        panic!("`mid`'s body should apply `h`");
    };
    assert_eq!(
        mid_holes[0].0, head.id,
        "the inherited hole belongs to `mid`'s use of `h`"
    );
    assert_ne!(
        mid_holes[0].0, h_holes[0].0,
        "the two definitions record their holes at different sites"
    );
}

/// The checker and the elaborator read the **same** AST, with the same
/// `ExprId`s.
///
/// This is the check four failed attempts at the last construct lacked. I
/// reasoned about the ids the walk sees versus the ids the checker recorded,
/// without first establishing that the two come from one tree. If they do, the
/// mismatch must have another cause; if they do not, nothing built on comparing
/// them can work.
///
/// Both sides go through `lower::parse_module`: the checker via
/// `parse_module(db, input.file(db))`, the elaborator via
/// `Workspace::modules[..].module`, which is built from the same query in
/// `Workspace::load`. This asserts that rather than assuming it.
#[test]
fn the_checker_and_the_elaborator_share_one_ast() {
    let src = "users : query { id = int, age = int } = table \"s\" \"users\"\n\
               h = e => users & select { x = e + 1, id = .id }\n\
               ok = h .age\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());

    // The checker's view, straight from the salsa query.
    let file = ws.inputs[ws.root].file(&ws.db);
    let from_query = crate::lower::parse_module(&ws.db, *file);
    // The elaborator's view, from the loaded module.
    let from_workspace = &ws.modules[ws.root].module;

    assert_eq!(
        from_query.module.defs.len(),
        from_workspace.defs.len(),
        "the two views disagree on how many definitions there are"
    );
    for (i, (a, b)) in from_query
        .module
        .defs
        .iter()
        .zip(&from_workspace.defs)
        .enumerate()
    {
        assert_eq!(a.name, b.name, "definition {i} differs by name");
        assert_eq!(
            a.body.id, b.body.id,
            "definition {i} (`{}`) has a different body id: {} vs {}",
            a.name, a.body.id, b.body.id
        );
        assert_eq!(
            a.body.span, b.body.span,
            "definition {i} (`{}`) has a different body span",
            a.name
        );
    }
}

/// The `ExprId`s of the exact program the failing test uses, printed.
///
/// The traces in the earlier attempts printed spans that matched no definition
/// in the program under test, which is what sent me looking for two ASTs. They
/// share one, so the spans must have come from a *different program* — another
/// test's elaboration, or the prelude's. This prints the ids for the one string
/// that matters, and locates the callee name node by walking the tree.
#[test]
fn report_the_ids_of_the_failing_program() {
    let ws = Workspace::from_source(
        "users : query { id = int, name = string, age = int, active = bool } = \
         table \"public\" \"users\"\n\
         orders : query { id = int, user_id = int, amount = float, status = string } = \
         table \"public\" \"orders\"\n\
         h = e => users & select { x = e + 1, id = .id }\n\
         q = h count\n\
         ok = h .age\n\
         win = h (rowNumber { order = [asc .id] })\n",
    );
    let tc = crate::check::check(&ws);
    // `q = h count` is *meant* to be rejected — the test is about `ok` and
    // `win` — so errors are expected here and the ids are printed regardless.
    for (i, d) in ws.modules[ws.root].module.defs.iter().enumerate() {
        eprintln!(
            "DEF {i} `{}` body.id={} span={:?} choices={:?} holes={}",
            d.name,
            d.body.id,
            d.body.span,
            tc.choices_of(ws.root, i),
            tc.holes(ws.root, i)
        );
        // The tree, so the recorded site can be located in it.
        fn walk(e: &cagara_syntax::ast::Expr, depth: usize) {
            use cagara_syntax::ast::ExprKind as K;
            let k = match &e.kind {
                K::App(..) => "App",
                K::Name(n) => return eprintln!("{}Name({n}) id={}", "  ".repeat(depth), e.id),
                K::Field(..) => "Field",
                K::Lambda(..) => "Lambda",
                K::Record(_) => "Record",
                K::List(_) => "List",
                _ => "other",
            };
            eprintln!("{}{k} id={}", "  ".repeat(depth), e.id);
            match &e.kind {
                K::App(f, a) => {
                    walk(f, depth + 1);
                    for x in a.iter() {
                        walk(x, depth + 1);
                    }
                }
                K::Lambda(_, b) => walk(b, depth + 1),
                K::Record(fs) => {
                    for (_, v) in fs {
                        walk(v, depth + 1);
                    }
                }
                K::List(xs) => {
                    for x in xs {
                        walk(x, depth + 1);
                    }
                }
                K::Proj(b, _) => walk(b, depth + 1),
                _ => {}
            }
        }
        walk(&d.body, 2);
    }
}

/// The same measurement, on the **exact** source `consistent_src` builds.
///
/// Every probe so far used a hand-written variant, whose ids and spans differ
/// from the test's. That mismatch is what made four traces lead nowhere: the
/// numbers were real but from another program. This uses the test's own string.
#[test]
fn report_the_ids_of_the_consistent_src_program() {
    const TABLES: &str =
        "users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n\
         orders : query { id = int, user_id = int, amount = float, status = string } = table \"public\" \"orders\"\n";
    let h = "h = e => users & select { x = e + 1, id = .id }\n";
    let src = format!(
        "{TABLES}{h}q = h count\nok = h .age\nwin = h (rowNumber {{ order = [asc .id] }})\n"
    );
    let ws = Workspace::from_source(&src);
    let tc = crate::check::check(&ws);
    for (i, d) in ws.modules[ws.root].module.defs.iter().enumerate() {
        eprintln!(
            "SRC-DEF {i} `{}` body.id={} span={:?} choices={:?}",
            d.name,
            d.body.id,
            d.body.span,
            tc.choices_of(ws.root, i)
        );
    }
}
