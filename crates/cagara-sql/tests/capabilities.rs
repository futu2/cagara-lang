//! The capability matrix: a dialect either accepts a construct or refuses it,
//! and **wrapping the construct in another stage must not change which**.
//!
//! A dialect refusal is a statement about the *construct* — SQLite has no
//! grouping sets, the MySQL family has no `GROUPS` frame, `arrayAgg` needs an
//! array type. It is not a statement about the stage the construct happens to
//! sit under. So if `f & agg { k = rollup .k, n = count }` is refused on SQLite,
//! then `f & agg { k = rollup .k, n = count } & agg { n2 = count }` must be
//! refused too: the second stage renames nothing and removes nothing, and the
//! subtotal rows it aggregates are still `ROLLUP` subtotals. Emitting SQL for
//! the second query means emitting `GROUP BY ROLLUP(k)` to an engine that has no
//! `ROLLUP`, and the user finds out at run time, from the engine, with a message
//! that names the generated SQL instead of the Cagara line that asked for it.
//!
//! That is the invariant this file pins, and it is deliberately one-directional
//! in its claims: it never says a particular construct *should* be refused on a
//! particular dialect (that is the per-dialect capability table's business, and
//! it is allowed to change). It says only that the answer for a construct and
//! the answer for the same construct with one more stage on the end are the same
//! answer. Both accepted and both refused are fine; a split is a bug.
//!
//! # Two invariants, because a construct can be missed two ways
//!
//! A capability check can fail to see a construct in either of two dimensions,
//! and each needs its own question:
//!
//! 1. **Downward.** The construct is in a stage that a *later* stage consumes.
//!    `a_wrapper_never_changes_which_dialects_accept_a_construct` asks whether
//!    appending a stage changes the answer.
//! 2. **Sideways.** The construct is in a node the check does not recognise as a
//!    place expressions live. `Rel::Update` is the case: it is not `Rel::Select`,
//!    so a check written as "match `Select`, `Agg`, else recurse into children"
//!    treats it as a leaf and never looks at its own fields.
//!    `a_construct_means_the_same_in_a_select_and_in_an_update` asks whether
//!    writing the same expression in a `select` and in an `update` gets the same
//!    answer.
//!
//! The second is not implied by the first: the wrapper test compares a query
//! against itself, so a construct that is *wrongly accepted* in the baseline is
//! wrongly accepted in the wrapper too and the two agree. Only a comparison
//! against a different placement can see that.
//!
//! # Why every wrapper here is faithful
//!
//! A wrapper can only be used if it cannot *legitimately* change the answer:
//!
//! * `select-identity` re-projects the construct's row unchanged (each construct
//!   spells its own, because the rows differ). Dropping a column would be a
//!   cheat — projecting away the `arrayAgg` column really does remove the `list`
//!   from the statement, and then a refusal would be wrong.
//! * `where`, `order`, `limit`, `distinct` are stages that pass the row through,
//!   so the construct's columns survive them untouched. `where` is written with a
//!   constant predicate (`where true`) so that it type-checks against every
//!   construct's row — a nullable column compared to a non-nullable one is a
//!   checker error, and a checker error is not a dialect answer. A constant
//!   predicate still produces the stage: the IR has a `Rel::Where` either way.
//! * `join` adds `g`'s columns to the construct's row rather than replacing it,
//!   and is likewise a constant predicate (`innerJoin g true`).
//! * `unionAll` needs a second operand with the same row, so every construct also
//!   gets an alias root query with the identical pipeline.
//! * `agg` is the exception that proves the rule. It *replaces* the row, so an
//!   `agg` that mentions only `count` drops every column below it — and a column
//!   nothing above it reads is never lowered at all. So each construct supplies
//!   its own `agg` that reads a column it produced (see [`Construct::agg`]); a
//!   single shared `agg { n2 = count }` would silently stop testing the
//!   construct rather than the wrapper, and the invariant would report a
//!   mismatch that is the test's fault.
//!
//! # What is deliberately *not* in the matrix
//!
//! Nothing is skipped silently, so the constructs and stages that were
//! considered and left out are listed here with the reason:
//!
//! * **`offset` on T-SQL.** `f & … & offset 1` is refused (`T-SQL needs an
//!   `order` before `offset``) but `f & … & offset 1 & limit 3` is accepted and
//!   emits `ORDER BY (SELECT NULL) OFFSET 1 ROWS FETCH NEXT 3 ROWS ONLY`. The
//!   two disagree, and the pair is left out of the matrix because the
//!   disagreement is not the bug this file is about. `offset` is a *stage*, not
//!   a construct hidden under one, so no capability check has to descend to find
//!   it; the refusal comes from the paging rewrite, which fires only when there
//!   is an `OFFSET` and no `LIMIT` — with a `LIMIT`, sqlglot supplies the dummy
//!   `ORDER BY` itself, and without one the compiler asks the reader to write
//!   the order instead of inventing one. The refusal is also in the safe
//!   direction: the *simpler* query is refused and the accepted one is valid
//!   T-SQL, so nothing compiles and then fails on the server. It is an
//!   inconsistency worth knowing about, but it is not a wrong answer.
//! * **A window in a `where`, a join `on`, an `order` key, or a partition key.**
//!   The checker refuses all four before the backend is reached (`where` cannot
//!   filter on a window function; join predicates cannot contain aggregates or
//!   window functions; sort and partition keys must be plain column
//!   expressions), so there is no dialect answer to compare. That is also why
//!   `Rel::Where` and the join `on` are not exercised by the placement test: the
//!   only node besides `Rel::Select` that can legally carry a window is
//!   `Rel::Update`, and that is the one the test uses.
//! * **`semiJoin` / `antiJoin` with a nullable-keyed construct on the right.**
//!   The right operand of a semi join is checked against the left row's nullability,
//!   so wrapping a `rollup` (whose key is `maybe int`) that way is a type error
//!   rather than a dialect question. The construct would have to be re-keyed to a
//!   non-nullable column, which changes the construct.
//!
//! Nothing here is skipped and nothing is `#[ignore]`d: a construct that could
//! not be expressed would be a compile error in this file, not a silent gap.
//!
//! Pure text test: no engine shell, no `CAGARA_NO_ENGINES`, no network. It runs
//! in the default CI job. One workspace is built per dialect rather than one per
//! query — building a `Workspace` is the expensive part, and a thousand of them
//! would put a minute of parser work in a push.

use cagara_hir::{root_queries, Workspace};
use cagara_sql::{compile, dialect, Dialect, Options};

/// The tables every construct reads.
///
/// Two, because one of the wrappers is a join. `g`'s key is `gk` rather than `k`
/// so that the joined row has no colliding name (the checker warns about those,
/// and a warning is noise in a test that is about refusals).
const SCHEMA: &str = "\
f : query { k = int, n = int, s = string } = table \"\" \"f\"
g : query { gk = int, m = int } = table \"\" \"g\"
";

/// The dialects the matrix covers: the one the IR is written in, plus every
/// dialect `cagara_sql` documents a distinct rule for. `dialect` is the public
/// lookup, so a name that stops resolving fails this test rather than silently
/// dropping a column of the matrix.
const DIALECTS: &[&str] = &[
    "ansi",
    "sqlite",
    "mysql",
    "tsql",
    "duckdb",
    "postgres",
    "bigquery",
    "snowflake",
    "trino",
    "spark",
];

/// One construct whose dialect support is a question, and the pipeline that
/// produces it.
///
/// `core` is the query itself — the construct sits where the stage that owns it
/// sits, with nothing after it. That matters: the interesting cases are the ones
/// where the *unwrapped* query is already refused, so the construct has to be
/// visible to whatever checks the dialect before the wrapper is added.
struct Construct {
    /// The name used in failure messages.
    name: &'static str,
    /// The pipeline, without the trailing `;` or any wrapper.
    core: &'static str,
    /// A `select` stage that reproduces `core`'s row exactly: same names, same
    /// types, same order. This is the faithful no-op wrapper.
    identity: &'static str,
    /// An `agg` stage that **reads a column the construct produced**.
    ///
    /// `agg` is the one wrapper that cannot be written once for the whole table:
    /// it replaces the row, so a stage that mentions only `count` drops every
    /// other column, and a column that nothing above it reads is never lowered.
    /// `f & select { n = coalesce 0 (tryInt .s) } & agg { n2 = count }` emits
    /// `SELECT COUNT(*) FROM f` — the `tryInt` is gone, so PostgreSQL's refusal
    /// of `TRY_CAST` has nothing left to refuse. That is the compiler being
    /// right (dead code is not SQL) and the *wrapper* being unfaithful, so each
    /// construct supplies an aggregate over its own value: `countOf .n` survives
    /// into the statement as `COUNT(COALESCE(TRY_CAST(s AS BIGINT), 0))`, and
    /// the construct is still there for the dialect check to see.
    ///
    /// `countOf` rather than `max` because it is defined for every type,
    /// including the `list` that `arrayAgg` and `unnest` produce.
    agg: &'static str,
}

const CONSTRUCTS: &[Construct] = &[
    Construct {
        name: "fullJoin",
        core: "f & fullJoin g (.<k == .>gk)",
        // `fullJoin` null-extends both sides, so every column is `maybe`.
        identity: "select { k = .k, n = .n, s = .s, gk = .gk, m = .m }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        name: "rollup",
        core: "f & agg { k = rollup .k, n = count }",
        identity: "select { k = .k, n = .n }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        name: "cube",
        core: "f & agg { k = cube .k, n = count }",
        identity: "select { k = .k, n = .n }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        name: "groups-frame",
        core: "f & select { k = .k, n = .n, s = over { order = [asc .k], \
               frame = groups (preceding 1) currentRow } (sum .n) }",
        identity: "select { k = .k, n = .n, s = .s }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        name: "range-frame",
        core: "f & select { k = .k, n = .n, s = over { order = [asc .k], \
               frame = range (preceding 1) currentRow } (sum .n) }",
        identity: "select { k = .k, n = .n, s = .s }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        // A window, then a filter that reads it: `QUALIFY` on the four targets
        // that have it, a derived table everywhere else. The filter is the point
        // — it is what forces the window to be materialized rather than inlined.
        name: "qualify",
        core: "f & select { k = .k, n = .n, s = over { order = [asc .k] } (sum .n) } \
               & where (coalesce 0 .s > 0)",
        identity: "select { s = .s, k = .k, n = .n }",
        agg: "agg { n2 = count, kept = countOf .s }",
    },
    Construct {
        name: "filtered-agg",
        core: "f & agg { k = group .k, n = filtered (.n > 0) (sum .n) }",
        identity: "select { k = .k, n = .n }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        name: "arrayAgg",
        core: "f & agg { k = group .k, n = count, xs = arrayAgg .n }",
        // `xs` stays in the row on purpose: a projection that dropped it would
        // remove the `list` from the statement, and the refusal would be gone
        // for a reason that has nothing to do with the wrapper.
        identity: "select { k = .k, n = .n, xs = .xs }",
        agg: "agg { n2 = count, kept = countOf .xs }",
    },
    Construct {
        name: "tryInt",
        core: "f & select { k = .k, n = coalesce 0 (tryInt .s) }",
        identity: "select { k = .k, n = .n }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
    Construct {
        // `unnest` is the one construct here that is a *relational node* rather
        // than an expression, so its refusal is checked in a different place
        // from the frame and grouping-set rules. It belongs in the matrix for
        // exactly that reason: a per-construct check that walks the tree and one
        // that does not are two different ways to be wrong, and the wrapper has
        // to be invisible to both.
        name: "unnest",
        core: "f & agg { k = group .k, xs = arrayAgg .n } & unnest .xs",
        identity: "select { k = .k, xs = .xs, value = .value }",
        agg: "agg { n2 = count, kept = countOf .value }",
    },
    Construct {
        // A `groups` frame in an `update` field. On its own this row cannot
        // break the wrapper invariant — if the check cannot see `Rel::Update`
        // then the baseline is wrong and every wrapper is wrong in the same
        // way, and they agree. It is here for the other direction: once the
        // check does see `Update`, a later regression that only handles the
        // top-level node would show up as the wrapper disagreeing. The
        // *placement* bug it describes is caught by the second test below.
        name: "update-frame",
        core: "f & update { s = over { order = [asc .k], \
               frame = groups (preceding 1) currentRow } (sum .n) }",
        identity: "select { k = .k, n = .n, s = .s }",
        agg: "agg { n2 = count, kept = countOf .n }",
    },
];

/// The stage appended to a construct's pipeline.
enum Stage {
    /// A stage written out here, appended verbatim.
    Literal(&'static str),
    /// The construct's own `identity` select.
    Identity,
    /// The construct's own `agg` stage, which reads a column it produced.
    Agg,
    /// `& unionAll <the same pipeline>`, under the alias root query.
    UnionAllSame,
}

struct Wrapper {
    name: &'static str,
    stage: Stage,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "select-identity",
        stage: Stage::Identity,
    },
    Wrapper {
        name: "where",
        stage: Stage::Literal("where true"),
    },
    Wrapper {
        // Per construct: see `Construct::agg` for why this cannot be a constant.
        name: "agg",
        stage: Stage::Agg,
    },
    Wrapper {
        name: "order",
        stage: Stage::Literal("order [asc .k]"),
    },
    Wrapper {
        name: "limit",
        stage: Stage::Literal("limit 3"),
    },
    Wrapper {
        name: "distinct",
        stage: Stage::Literal("distinct"),
    },
    Wrapper {
        name: "join",
        stage: Stage::Literal("innerJoin g true"),
    },
    Wrapper {
        name: "unionAll",
        stage: Stage::UnionAllSame,
    },
];

/// Why a query did not produce SQL.
enum Failure {
    /// The Cagara source itself does not check. Never a dialect answer, and
    /// never acceptable here: it would mean the wrapper is not faithful.
    Check(String),
    /// The compiler refused to lower it for this dialect. This is the answer the
    /// invariant is about.
    Dialect(String),
}

/// One query to compile, and the name it has in its workspace.
struct Case {
    /// The Cagara definition name, unique inside the batch.
    def: String,
    /// The pipeline assigned to it.
    pipeline: String,
    /// Extra definitions the pipeline refers to.
    helpers: Vec<(String, String)>,
}

/// Compile every case in one workspace, for one dialect.
///
/// One workspace per dialect rather than one per query: parsing and checking is
/// the expensive half of this test, and it is per file. The root queries come
/// back in definition order, so they are matched to the cases by name.
fn compile_batch(cases: &[Case], d: Dialect) -> Vec<Result<String, Failure>> {
    let mut src = String::from(SCHEMA);
    for c in cases {
        src.push_str(&format!("\n{} = {}\n", c.def, c.pipeline));
        for (name, pipeline) in &c.helpers {
            src.push_str(&format!("{name} = {pipeline}\n"));
        }
    }
    let ws = Workspace::from_source(&src);
    let by_name: std::collections::HashMap<String, _> = root_queries(&ws).into_iter().collect();
    cases
        .iter()
        .map(|c| {
            let rel = by_name
                .get(&c.def)
                .unwrap_or_else(|| panic!("`{}` is not a root query of the batch", c.def))
                .as_ref()
                .map_err(|diag| Failure::Check(diag.message.clone()))?;
            compile(
                rel,
                Options {
                    dialect: d,
                    pretty: false,
                    optimize: false,
                },
            )
            .map_err(Failure::Dialect)
        })
        .collect()
}

/// One side of a comparison, for the failure message.
fn describe(outcome: &Result<String, Failure>) -> String {
    match outcome {
        Ok(sql) => format!("Ok({sql:?})"),
        Err(Failure::Dialect(m)) => format!("Err({m:?})"),
        Err(Failure::Check(m)) => format!("CheckErr({m:?})"),
    }
}

#[test]
fn a_wrapper_never_changes_which_dialects_accept_a_construct() {
    // One batch, laid out so that case `2 * i + 1` is construct `i` unwrapped and
    // cases `2 * (i * WRAPPERS.len() + j) + 2` onward are its wrappers. Rather
    // than encode that arithmetic, the batch is built alongside an index.
    let mut cases: Vec<Case> = Vec::new();
    // (construct index, wrapper index or None for the baseline) -> case index.
    let mut at: Vec<(usize, Option<usize>)> = Vec::new();
    for (i, c) in CONSTRUCTS.iter().enumerate() {
        let def = format!("b{i}");
        cases.push(Case {
            def: def.clone(),
            pipeline: c.core.to_string(),
            helpers: vec![],
        });
        at.push((i, None));
        for (j, w) in WRAPPERS.iter().enumerate() {
            let alias = format!("a{i}");
            let (pipeline, helpers) = match &w.stage {
                Stage::Identity => (format!("{} & {}", c.core, c.identity), vec![]),
                Stage::Agg => (format!("{} & {}", c.core, c.agg), vec![]),
                Stage::Literal(suffix) => (format!("{} & {suffix}", c.core), vec![]),
                Stage::UnionAllSame => (
                    format!("{} & unionAll {alias}", c.core),
                    vec![(alias, c.core.to_string())],
                ),
            };
            cases.push(Case {
                def: format!("w{i}_{j}"),
                pipeline,
                helpers,
            });
            at.push((i, Some(j)));
        }
    }

    let mut mismatches: Vec<String> = Vec::new();
    let mut refusals = 0usize;
    let mut triples = 0usize;

    for name in DIALECTS {
        let d =
            dialect(name).unwrap_or_else(|| panic!("`{name}` is not a dialect this crate knows"));
        let results = compile_batch(&cases, d);
        assert_eq!(results.len(), at.len(), "batch and index disagree");
        for (k, (i, wrapper)) in at.iter().enumerate() {
            if let Err(Failure::Check(m)) = &results[k] {
                panic!(
                    "construct `{}`{} does not check, so the comparison would be \
                     meaningless — the wrapper is not faithful: {m}\n\n{}",
                    CONSTRUCTS[*i].name,
                    wrapper
                        .map(|j| format!(" + wrapper `{}`", WRAPPERS[j].name))
                        .unwrap_or_default(),
                    cases[k].pipeline,
                );
            }
        }
        for (i, c) in CONSTRUCTS.iter().enumerate() {
            // The baseline: construct `i`, case index `i * (WRAPPERS.len() + 1)`.
            let base = i * (WRAPPERS.len() + 1);
            let unwrapped = &results[base];
            if unwrapped.is_err() {
                refusals += 1;
            }
            for (j, w) in WRAPPERS.iter().enumerate() {
                triples += 1;
                let wrapped = &results[base + 1 + j];
                // Both accepted, or both refused *by the dialect*. A refusal for
                // another reason on one side only is a mismatch too: it means the
                // wrapper changed whether the compiler understood the program.
                let agree = matches!(
                    (unwrapped, wrapped),
                    (Ok(_), Ok(_)) | (Err(Failure::Dialect(_)), Err(Failure::Dialect(_)))
                );
                if !agree {
                    mismatches.push(format!(
                        "construct=`{}` wrapper=`{}` dialect=`{}`: unwrapped={} wrapped={}",
                        c.name,
                        w.name,
                        name,
                        describe(unwrapped),
                        describe(wrapped),
                    ));
                }
            }
        }
    }

    // A matrix in which nothing is ever refused proves nothing: every cell would
    // agree because every cell is `Ok`. This is a guard on the matrix, not an
    // expectation about any one dialect.
    assert!(
        refusals > 0,
        "vacuous matrix: no construct was refused by any dialect, so `wrapped == \
         unwrapped` holds trivially"
    );

    assert!(
        mismatches.is_empty(),
        "{} of {} (construct, wrapper, dialect) triples break the invariant — a dialect \
         refuses a construct on its own but accepts it once one more stage is appended \
         (or the reverse):\n\n{}\n\n\
         coverage: {} constructs x {} wrappers x {} dialects = {} triples, \
         {} of {} (construct, dialect) pairs refused\n",
        mismatches.len(),
        triples,
        mismatches.join("\n"),
        CONSTRUCTS.len(),
        WRAPPERS.len(),
        DIALECTS.len(),
        triples,
        refusals,
        CONSTRUCTS.len() * DIALECTS.len(),
    );
}

/// One expression whose dialect support is a question, and the two stage nodes
/// that can carry it.
///
/// A window expression is legal in a `select` field and in an `update` field,
/// and the two produce the same SQL for the same expression. So a target that
/// cannot express a frame unit cannot express it in either place: the answer has
/// to be the same. A check that walks only `Rel::Select` and `Rel::Agg` and
/// treats every other node as a leaf answers `OK` for the `update` spelling of a
/// query it refuses in the `select` spelling, and the user gets SQL their engine
/// rejects.
///
/// `Rel::Update` is the interesting node because it is *not* a `Select` — it is
/// the shape a check written as "match `Select`, `Agg`, else recurse into
/// children" gets wrong without noticing, since it never looks at the update's
/// own expressions at all. `Rel::Order` is not in the table because the checker
/// rejects a window in a sort key outright (`sort and partition keys must be
/// plain column expressions`), and a window in a `where` or a join `on` is
/// rejected the same way, so there is no query to compare for those.
const WINDOW_EXPRS: &[(&str, &str)] = &[
    (
        "rows-frame",
        "over { order = [asc .k], frame = rows (preceding 1) currentRow } (sum .n)",
    ),
    (
        "range-frame",
        "over { order = [asc .k], frame = range (preceding 1) currentRow } (sum .n)",
    ),
    (
        "groups-frame",
        "over { order = [asc .k], frame = groups (preceding 1) currentRow } (sum .n)",
    ),
    // No frame at all, so nothing about a frame unit is being asked. It is the
    // control: it is accepted everywhere, so a mismatch here would mean the
    // comparison machinery is broken rather than that the compiler is.
    ("no-frame", "over { order = [asc .k] } (sum .n)"),
];

#[test]
fn a_construct_means_the_same_in_a_select_and_in_an_update() {
    // Both spellings produce the same row `{ k, n, s }`, so neither is the
    // "cheaper" query: `select` renames nothing and drops nothing, and `update`
    // appends `s` because the input has no such column.
    let mut cases: Vec<Case> = Vec::new();
    for (i, (_, expr)) in WINDOW_EXPRS.iter().enumerate() {
        cases.push(Case {
            def: format!("s{i}"),
            pipeline: format!("f & select {{ k = .k, n = .n, s = {expr} }}"),
            helpers: vec![],
        });
        cases.push(Case {
            def: format!("u{i}"),
            pipeline: format!("f & update {{ s = {expr} }}"),
            helpers: vec![],
        });
    }

    let mut mismatches: Vec<String> = Vec::new();
    let mut refusals = 0usize;
    let mut pairs = 0usize;

    for name in DIALECTS {
        let d =
            dialect(name).unwrap_or_else(|| panic!("`{name}` is not a dialect this crate knows"));
        let results = compile_batch(&cases, d);
        for (i, (expr, _)) in WINDOW_EXPRS.iter().enumerate() {
            let in_select = &results[2 * i];
            let in_update = &results[2 * i + 1];
            for (side, outcome) in [("select", in_select), ("update", in_update)] {
                if let Err(Failure::Check(m)) = outcome {
                    panic!("`{expr}` in a `{side}` does not check: {m}");
                }
            }
            pairs += 1;
            if in_select.is_err() {
                refusals += 1;
            }
            let agree = matches!(
                (in_select, in_update),
                (Ok(_), Ok(_)) | (Err(Failure::Dialect(_)), Err(Failure::Dialect(_)))
            );
            if !agree {
                mismatches.push(format!(
                    "expr=`{expr}` dialect=`{name}`: in select={} in update={}",
                    describe(in_select),
                    describe(in_update),
                ));
            }
        }
    }

    assert!(
        refusals > 0,
        "vacuous: no window expression was refused in the `select` spelling, so there is \
         nothing for the `update` spelling to disagree with"
    );

    assert!(
        mismatches.is_empty(),
        "{} of {} (expression, dialect) pairs give different answers depending on \
         whether the expression is written in a `select` or an `update` — a window is the \
         same window in both, so the dialect answer must be too:\n\n{}\n\n\
         coverage: {} expressions x {} dialects = {} pairs, {} refused in the `select` \
         spelling\n",
        mismatches.len(),
        pairs,
        mismatches.join("\n"),
        WINDOW_EXPRS.len(),
        DIALECTS.len(),
        pairs,
        refusals,
    );
}
