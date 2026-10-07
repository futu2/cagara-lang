//! Elaboration tests: the evaluator's by-product is a `CoreTerm` tree.
//!
//! Two jobs, and they are different jobs:
//!
//! 1. **Structure.** Every assertion below inspects the *shape* of the tree —
//!    which named constructor was called, with which arguments, nested how.
//!    That is the point of the `CoreTerm` step: a `Prim` is classified and
//!    handed to an explicit constructor, so the relational structure is
//!    visible in the value the evaluator returns instead of only appearing as
//!    a `Rel` node assembled from a name-keyed dispatch table.
//!
//! 2. **Argument order.** `prims::call` receives a `Vec<Value>` and saturates
//!    on `Prim::arity()`, so reading a primitive's arguments in the wrong
//!    order compiles cleanly and fails only at run time. That is a real bug
//!    this file exists to catch (see the stage-form tests at the bottom).
//!
//! **What these tests deliberately do not cover.** `CoreTerm` is the *row-less*
//! twin of `CheckedQuery`, so it cannot express a wrong column type, a missing
//! `maybe` on an outer join's nullable side, or `update` merging in the wrong
//! precedence. Those are row-level laws, they are invisible in this tree, and
//! they need `CheckedQuery.row` / `--types` assertions. Nothing here should be
//! read as standing in for them.

use crate::core_term::CoreTerm;
use crate::workspace::Workspace;
use crate::ir::{JoinKind, Lit};
use crate::ir::Rel;
use crate::workspace::Diag;

/// The elaborated core term of one query definition of `src`.
fn core_of(src: &str, name: &str) -> CoreTerm {
    let ws = Workspace::from_source(src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = crate::check::check(&ws);
    let type_errors: Vec<String> = tc.errors.iter().map(|e| e.diag.message.clone()).collect();
    assert!(type_errors.is_empty(), "{type_errors:?}");
    crate::eval::root_core_terms(&ws, &tc)
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no query definition `{name}`"))
        .1
        .unwrap_or_else(|d| panic!("`{name}` failed to elaborate: {}", d.message))
}

/// A term without `At` wrappers, so an assertion about structure is not
/// disturbed by where a stage was written.
fn bare(t: &CoreTerm) -> &CoreTerm {
    t.bare()
}

/// The `Table` name of a term.
fn table_name(t: &CoreTerm) -> String {
    match bare(t) {
        CoreTerm::Table { name, .. } => name.clone(),
        o => panic!("expected a table, found {}", o.kind()),
    }
}

/// The `(schema, name)` of a table term.
fn qualified(t: &CoreTerm) -> (String, String) {
    match bare(t) {
        CoreTerm::Table { schema, name, .. } => (schema.clone(), name.clone()),
        o => panic!("expected a table, found {}", o.kind()),
    }
}

/// `(column, asc)` of each `order` key, unwrapping the `Dir` node.
fn order_keys(t: &CoreTerm) -> Vec<(String, bool)> {
    match bare(t) {
        CoreTerm::Order { keys, .. } => keys
            .iter()
            .map(|(k, asc)| {
                let e = match k {
                    CoreTerm::Dir { expr, .. } => expr.as_ref(),
                    o => o,
                };
                (column_of(e), *asc)
            })
            .collect(),
        o => panic!("expected an order, found {}", o.kind()),
    }
}

/// The column a term reads, as its name.
fn column_of(t: &CoreTerm) -> String {
    match bare(t) {
        CoreTerm::Col(_, n) => n.clone(),
        o => panic!("expected a column, found {}", o.kind()),
    }
}

const TABLES: &str = "users : query { id = int, name = string, age = int, active = bool } = \
                       table \"public\" \"users\"\n\
                       orders : query { id = int, user_id = int, amount = float, status = string, \
                       created_at = date } = table \"public\" \"orders\"\n";

// ── (a) `examples/report.cagara`'s `user_totals` ───────────────────────────
//
//   user_totals = users
//     & leftJoin orders (.<id == .>user_id)
//     & agg {
//       id = group .id,
//       total = coalesce 0.0 (sum (coalesce 0.0 .amount)),
//       orders = coalesce 0 (sum (ifThenElse (isNotNull .user_id) 1 0))
//     }
//
// The shape is `Agg(Join { Left, .. }, [group, coalesce(sum(..))])`: a left
// join under an aggregate, with the coalesce written *inside* the field
// expressions. Asserting the nesting is the point — a rename of `Rel::Agg`
// would not produce a left join here.
//
// The *side-sensitive* assertions (`Left` kind, `users` on the left, `orders`
// on the right, `.<id` left-qualified, `.>user_id` right-qualified) are there
// because a plain node-kind check would survive swapping the two join inputs.

#[test]
fn left_join_agg_coalesce_elaborates_to_a_named_core_tree() {
    let src = format!(
        "{TABLES}\
         user_totals = users\n\
         \x20 & leftJoin orders (.<id == .>user_id)\n\
         \x20 & agg {{ id = group .id, total = coalesce 0.0 (sum (coalesce 0.0 .amount)) }}\n"
    );
    let t = core_of(&src, "user_totals");

    // The outermost stage is the aggregate, over a left join.
    let CoreTerm::Agg { input, fields: fs } = bare(&t) else {
        panic!("expected `Agg` at the root, found {}", bare(&t).kind());
    };
    let CoreTerm::Join {
        kind,
        left,
        right,
        on,
    } = bare(input)
    else {
        panic!(
            "expected the aggregate's input to be a `Join`, found {}",
            input.kind()
        );
    };

    // The join kind is the *constructor argument*, not something rediscovered.
    assert_eq!(*kind, JoinKind::Left);
    assert_eq!(table_name(left), "users");
    assert_eq!(table_name(right), "orders");

    // The predicate is a template over two side-qualified columns. The sides
    // are what makes this assertion side-sensitive: exchanging the join inputs
    // would put `id` on the right and `user_id` on the left.
    let CoreTerm::Tpl { args, .. } = bare(on) else {
        panic!("expected a template predicate, found {}", on.kind());
    };
    assert_eq!(args.len(), 2, "`.<id == .>user_id` has two operands");
    assert!(
        matches!(bare(&args[0]), CoreTerm::Col(cagara_syntax::ast::Side::Left, n) if n == "id")
    );
    assert!(
        matches!(bare(&args[1]), CoreTerm::Col(cagara_syntax::ast::Side::Right, n) if n == "user_id")
    );

    // `id = group .id` is a `group` term; `total` is a coalesce template whose
    // argument is the nested `sum`.
    let names: Vec<&str> = fs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["id", "total"]);
    assert!(matches!(bare(&fs[0].1), CoreTerm::Group(_)));

    let CoreTerm::Tpl { args, .. } = bare(&fs[1].1) else {
        panic!(
            "expected the coalesce to be a template, found {}",
            fs[1].1.kind()
        );
    };
    assert_eq!(args.len(), 2);
    assert!(
        matches!(bare(&args[0]), CoreTerm::Lit(Lit::Float(f)) if f == "0.0"),
        "the coalesce default is a float literal"
    );
    // `sum` is an aggregate template wrapping another coalesce — the depth-1
    // rule means it is `AggExpr`, not a nested `Agg`.
    let CoreTerm::AggExpr { args, .. } = bare(&args[1]) else {
        panic!(
            "expected `sum ..` to be an aggregate template, found {}",
            args[1].kind()
        );
    };
    let CoreTerm::Tpl { args, .. } = bare(&args[0]) else {
        panic!("expected the inner coalesce to be a template");
    };
    assert_eq!(column_of(&args[1]), "amount");
}

// ── (b) a two-sided join with `where` / `order` / `limit` ──────────────────
//
// Each stage must appear as its own named constructor, in pipeline order.

#[test]
fn where_order_limit_elaborate_to_distinct_constructors() {
    let src = format!(
        "{TABLES}\
         q = users\n\
         \x20 & innerJoin orders (.<id == .>user_id)\n\
         \x20 & where (.amount > 10.0)\n\
         \x20 & order [desc .name]\n\
         \x20 & limit 5\n"
    );
    let t = core_of(&src, "q");

    // Pipeline order, outermost first: limit <- order <- where <- join.
    let CoreTerm::Limit { input, n } = bare(&t) else {
        panic!("expected `Limit` at the root, found {}", bare(&t).kind());
    };
    assert_eq!(*n, 5);

    let (order_node, input) = match bare(input) {
        o @ CoreTerm::Order { input, .. } => (o, input),
        o => panic!("expected `Order` under the limit, found {}", o.kind()),
    };
    assert_eq!(
        order_keys(order_node),
        [("name".to_string(), false)],
        "`desc .name`"
    );

    let CoreTerm::Where { input, pred } = bare(input) else {
        panic!("expected `Where` under the order, found {}", input.kind());
    };
    // The predicate is a template whose first operand reads a column of the
    // join's right input — the wrap-around the checker proves.
    let CoreTerm::Tpl { args, .. } = bare(pred) else {
        panic!("expected a template predicate, found {}", pred.kind());
    };
    assert_eq!(column_of(&args[0]), "amount");
    assert!(matches!(bare(&args[1]), CoreTerm::Lit(Lit::Float(f)) if f == "10.0"));

    let CoreTerm::Join {
        kind, left, right, ..
    } = bare(input)
    else {
        panic!("expected `Join` under the where, found {}", input.kind());
    };
    assert_eq!(*kind, JoinKind::Inner);
    assert_eq!(table_name(left), "users");
    assert_eq!(table_name(right), "orders");
}

// ── the classification itself is exhaustive and total ──────────────────────

#[test]
fn every_primitive_is_classified_and_no_relational_prim_is_type_level() {
    use crate::prims::Kind;
    use crate::value::{Prim, PRIMS};

    // Every primitive in the table has a kind, and the relational half is
    // exactly the query half — so a relational primitive cannot quietly
    // become a scalar or a type-level no-op.
    for (name, p) in PRIMS {
        let kind = p.classify();
        let relational = matches!(
            p,
            Prim::Table
                | Prim::Where
                | Prim::Select
                | Prim::Update
                | Prim::Omit
                | Prim::Prefix
                | Prim::Suffix
                | Prim::AggStage
                | Prim::Order
                | Prim::Limit
                | Prim::Offset
                | Prim::Distinct
                | Prim::Join(_)
                | Prim::Set(_)
        );
        assert_eq!(
            relational,
            kind == Kind::Query,
            "`{name}` is relational iff it is classified `Query`"
        );
        assert!(
            !relational || kind != Kind::TypeLevel,
            "`{name}` is relational but classified type-level"
        );
        // The scalar and frame half keeps its own kind.
        if matches!(p, Prim::In | Prim::Group) {
            assert_eq!(kind, Kind::Expr, "`{name}` is scalar lowering");
        }
        if matches!(p, Prim::Asc | Prim::Desc) {
            assert_eq!(kind, Kind::Key, "`{name}` is a sort key");
        }
        if matches!(p, Prim::Rows) {
            assert_eq!(kind, Kind::Frame, "`{name}` is a window frame");
        }
    }

    // The two type-level operations keep returning their explanatory errors,
    // rather than being silently accepted.
    assert_eq!(Prim::MapValue.classify(), Kind::TypeLevel);
    assert_eq!(Prim::Merge.classify(), Kind::TypeLevel);
    let e = match crate::prims::call(Prim::MapValue, vec![]) {
        Err(e) => e.message,
        Ok(_) => panic!("`mapValue` must not evaluate"),
    };
    assert!(e.contains("type-level operation"), "{e}");
    let e = match crate::prims::call(Prim::Merge, vec![]) {
        Err(e) => e.message,
        Ok(_) => panic!("`merge` must not evaluate"),
    };
    assert!(e.contains("type-level operation"), "{e}");
}

// ── stage forms: each primitive's argument order, pinned ───────────────────
//
// `prims::call` takes a `Vec<Value>` and saturates on `arity()`. Reading the
// stage argument and the query in the wrong order therefore type-checks,
// compiles, and fails only at run time with "expected a query, found a
// record" — which is exactly what happened when this rewrite landed.
//
// Each test drives one primitive through the prelude's curried signature and
// asserts on something that depends on **which side each argument landed on**,
// not merely that evaluation succeeded. That distinction matters for the
// primitives whose two arguments have the same `Value` kind:
//
//   * `distinct` (arity 1): nothing to permute.
//   * `join (right, on, left)`: `right`/`left` are both queries, so a bare
//     `is_ok()` would not notice them exchanged — the test asserts which table
//     is on which side, and that `on` (an expression) landed in the predicate
//     position.
//   * `set (left, right)`: both are queries; the test gives the two inputs
//     different shapes so exchanging them changes the asserted tree.
//   * `table (schema, name)`: both are strings; the test asserts the qualified
//     pair, so a swap is visible.

const ONE_TABLE: &str =
    "t : query { id = int, name = string, age = int } = table \"public\" \"users\"\n";

/// The single root query of `body`, elaborated.
fn stage_query(body: &str) -> CoreTerm {
    core_of(&format!("{ONE_TABLE}q = {body}\n"), "q")
}

#[test]
fn where_stage_takes_its_predicate_first() {
    // `where : expr r bool -> query r -> query r`
    let t = stage_query("t & where (.age > 1)");
    let CoreTerm::Where { input, pred } = bare(&t) else {
        panic!("expected `Where`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    // The predicate really is the predicate: it reads `.age`. Reversing the
    // reads hands the expression to `core_query` and fails before here.
    let CoreTerm::Tpl { args, .. } = bare(pred) else {
        panic!("expected a template predicate, found {}", pred.kind());
    };
    assert_eq!(column_of(&args[0]), "age");
}

#[test]
fn select_stage_takes_its_fields_first() {
    // `select : expr r (row s) -> query r -> query s`
    let t = stage_query("t & select { id = .id, dn = .name }");
    let CoreTerm::Select { input, fields } = bare(&t) else {
        panic!("expected `Select`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["id", "dn"]);
    assert_eq!(column_of(&fields[0].1), "id");
    assert_eq!(column_of(&fields[1].1), "name");
}

#[test]
fn update_stage_takes_its_fields_first() {
    // `update : expr r (row s) -> query r -> query (merge r s)`
    let t = stage_query("t & update { age = .age + 1 }");
    let CoreTerm::Update { input, fields } = bare(&t) else {
        panic!("expected `Update`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].0, "age");
}

#[test]
fn omit_stage_takes_its_key_first() {
    // `omit : key_omit -> query r -> query s`
    let t = stage_query("t & omit \"name\"");
    let CoreTerm::Omit { input, key } = bare(&t) else {
        panic!("expected `Omit`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(key, "name");
}

#[test]
fn prefix_and_suffix_take_their_affix_first() {
    // `prefix : string -> query r -> query r`, and the same for `suffix`.
    // The affix is asserted literally, so a lost or swapped affix is visible.
    let t = stage_query("t & prefix \"u_\"");
    let CoreTerm::Prefix { input, affix } = bare(&t) else {
        panic!("expected `Prefix`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(affix, "u_");

    let t = stage_query("t & suffix \"_v2\"");
    let CoreTerm::Suffix { input, affix } = bare(&t) else {
        panic!("expected `Suffix`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(affix, "_v2");
}

#[test]
fn agg_stage_takes_its_fields_first() {
    // `agg : expr r (row s) -> query r -> query s`
    let t = stage_query("t & agg { n = count, a = sum .age }");
    let CoreTerm::Agg { input, fields } = bare(&t) else {
        panic!("expected `Agg`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["n", "a"]);
}

#[test]
fn order_stage_takes_its_keys_first() {
    // `order : list (expr r a) -> query r -> query r`
    let t = stage_query("t & order [desc .age, .name]");
    let CoreTerm::Order { input, keys } = bare(&t) else {
        panic!("expected `Order`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    // Direction is asserted too: a lost `desc` flag would still be an `Order`.
    assert_eq!(
        order_keys(&t),
        [("age".to_string(), false), ("name".to_string(), true)]
    );
    assert_eq!(keys.len(), 2);
}

#[test]
fn limit_and_offset_take_their_count_first() {
    // `limit : int -> query r -> query r`, and the same for `offset`. The
    // distinct counts keep the two stages from being confused.
    let t = stage_query("t & limit 3");
    let CoreTerm::Limit { input, n } = bare(&t) else {
        panic!("expected `Limit`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(*n, 3);

    let t = stage_query("t & offset 7");
    let CoreTerm::Offset { input, n } = bare(&t) else {
        panic!("expected `Offset`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
    assert_eq!(*n, 7);
}

#[test]
fn distinct_stage_takes_its_query() {
    // `distinct : query r -> query r`, arity 1: there is nothing to permute,
    // so this only pins that the single read is the query.
    let t = stage_query("t & distinct");
    let CoreTerm::Distinct { input } = bare(&t) else {
        panic!("expected `Distinct`, found {}", t.kind());
    };
    assert_eq!(table_name(input), "users");
}

/// The join and set forms need a second query, so they get their own source.
const TWO_TABLES: &str = "t : query { id = int, name = string } = table \"public\" \"users\"\n\
                          u : query { id = int, tag = string } = table \"public\" \"tags\"\n";

#[test]
fn join_stage_takes_right_then_on_then_left() {
    // `innerJoin : query r -> expr (row r <> row s) bool -> query r -> query s`
    // arrives as `[right, on, left]`; the original `prims::call` read them in
    // exactly that order, so this pins it.
    let src = format!("{TWO_TABLES}q = t & innerJoin u (.<id == .>id)\n");
    let t = core_of(&src, "q");
    let CoreTerm::Join {
        kind,
        left,
        right,
        on,
    } = bare(&t)
    else {
        panic!("expected `Join`, found {}", t.kind());
    };
    assert_eq!(*kind, JoinKind::Inner);
    // Left is the piped input (`t` → users), right is the join's own argument
    // (`u` → tags). Both are queries, so exchanging the reads keeps the tree a
    // `Join` — only these two assertions notice.
    assert_eq!(table_name(left), "users");
    assert_eq!(table_name(right), "tags");
    // `on` is a predicate: it must have landed in the predicate position, and
    // it is the side-qualified one, not a query.
    let CoreTerm::Tpl { args, .. } = bare(on) else {
        panic!("expected a template predicate, found {}", on.kind());
    };
    assert!(
        matches!(bare(&args[0]), CoreTerm::Col(cagara_syntax::ast::Side::Left, n) if n == "id")
    );
    assert!(
        matches!(bare(&args[1]), CoreTerm::Col(cagara_syntax::ast::Side::Right, n) if n == "id")
    );
}

#[test]
fn left_join_records_its_kind() {
    // The join kind is a constructor argument; a wrong kind here would
    // silently emit an inner join.
    let src = format!("{TWO_TABLES}q = t & leftJoin u (.<id == .>id)\n");
    let t = core_of(&src, "q");
    let CoreTerm::Join { kind, .. } = bare(&t) else {
        panic!("expected `Join`, found {}", t.kind());
    };
    assert_eq!(*kind, JoinKind::Left);
}

/// `(kind, left_table, right_table)` of a set operation, with each side's
/// source table read out of its own projection.
fn set_operands(t: &CoreTerm) -> (crate::ir::SetKind, String, String) {
    let CoreTerm::Set { kind, left, right } = bare(t) else {
        panic!("expected `Set`, found {}", t.kind());
    };
    (*kind, operand_table(left), operand_table(right))
}

/// Apply a set operation to two named tables *without* the pipe: the direct
/// curried call `op a b`. Each side is projected to the same row first, since
/// a set operation requires matching rows; the projection keeps each side's
/// source table visible, which is what the operand assertions read.
fn direct(op: &str) -> CoreTerm {
    let src = format!(
        "{TWO_TABLES}q = {op} (t & select {{ id = .id, v = .name }}) \
         (u & select {{ id = .id, v = .tag }})\n"
    );
    core_of(&src, "q")
}

/// Apply a set operation *through the pipe*: `a & op b`.
fn piped(op: &str) -> CoreTerm {
    let src = format!(
        "{TWO_TABLES}q = (t & select {{ id = .id, v = .name }}) \
         \x20 & {op} (u & select {{ id = .id, v = .tag }})\n"
    );
    core_of(&src, "q")
}

/// The table a set operation's operand ultimately reads, through its
/// projection.
fn operand_table(t: &CoreTerm) -> String {
    match bare(t) {
        CoreTerm::Select { input, .. } => table_name(input),
        o => table_name(o).to_string() + &format!(" <{}>", o.kind()),
    }
}

/// The direct form is the one the prelude's signature states: `op a b` puts
/// `a` on the left and `b` on the right. This is the form `fn`s and doctests
/// use, and it must not change.
#[test]
fn set_direct_form_puts_the_first_operand_on_the_left() {
    for (op, kind) in [
        ("union", crate::ir::SetKind::Union),
        ("unionAll", crate::ir::SetKind::UnionAll),
        ("intersect", crate::ir::SetKind::Intersect),
        ("except", crate::ir::SetKind::Except),
    ] {
        let (k, left, right) = set_operands(&direct(op));
        assert_eq!(k, kind, "`{op} t u` has kind {kind:?}");
        assert_eq!((left.as_str(), right.as_str()), ("users", "tags"), "`{op} t u`");
    }
}

/// The piped form, and the behaviour it actually has.
///
/// `union`/`intersect`/`except`/`unionAll` are plain primitives, not stage
/// operators, so `t & op u` is `_&_ t (op u)`: `op u` is a partial application
/// and the pipe supplies `t` as its **second** argument. The primitive
/// therefore receives `[u, t]`, so the operand written after `&` lands on the
/// **left** and the piped input lands on the **right**.
///
/// This is pre-existing behaviour, not something this refactor introduced, and
/// `docs/LEARN.md` promises the opposite ("the observable result follows the
/// left query"). For `union`/`unionAll`/`intersect` the reversal is invisible
/// because those are commutative; for **`except` it is a wrong answer** —
/// `users & except vips` computes `vips EXCEPT users`, the complement.
///
/// The test asserts the real order deliberately. It is the pin that a future
/// swap of `Set`'s two `next()` reads would trip, and it is the evidence for
/// whichever way the semantics are resolved (see `docs/implementation/`).
#[test]
fn set_piped_form_puts_the_piped_query_on_the_right() {
    for (op, kind) in [
        ("union", crate::ir::SetKind::Union),
        ("unionAll", crate::ir::SetKind::UnionAll),
        ("intersect", crate::ir::SetKind::Intersect),
        ("except", crate::ir::SetKind::Except),
    ] {
        let (k, left, right) = set_operands(&piped(op));
        assert_eq!(k, kind, "`t & {op} u` has kind {kind:?}");
        assert_eq!(
            (left.as_str(), right.as_str()),
            ("tags", "users"),
            "`t & {op} u`: the operand after `&` is the primitive's first \
             argument, and the piped input is the second"
        );
    }
}

/// The two forms must disagree — that is the whole point of pinning both. If a
/// future change made `t & op u` mean `op t u`, this fails and the piped set
/// operations become commutative-correct, which is what `LEARN.md` promises.
#[test]
fn set_piped_and_direct_forms_are_distinguishable() {
    let (_, piped_left, piped_right) = set_operands(&piped("except"));
    let (_, direct_left, direct_right) = set_operands(&direct("except"));
    assert_ne!(
        (piped_left.as_str(), piped_right.as_str()),
        (direct_left.as_str(), direct_right.as_str()),
        "`t & except u` and `except t u` currently pick opposite operands; \
         a test that could not see the difference would not have caught the \
         `except` reversal"
    );
}

#[test]
fn table_stage_takes_schema_then_name() {
    // `table : string -> string -> query r` — both arguments are strings, so
    // reversing them is invisible to `is_ok()`; the qualified pair is asserted.
    let t = stage_query("table \"public\" \"users\"");
    assert_eq!(qualified(&t), ("public".to_string(), "users".to_string()));
}

#[test]
fn in_takes_its_list_then_its_value() {
    // `in : list (expr r a) -> expr r a -> expr r bool` arrives as
    // `[list, value]`, matching the original `prims::call`.
    let t = stage_query("t & where (in [1, 2] .age)");
    let CoreTerm::Where { pred, .. } = bare(&t) else {
        panic!("expected `Where`, found {}", t.kind());
    };
    let CoreTerm::In { value, list, .. } = bare(pred) else {
        panic!("expected `In`, found {}", pred.kind());
    };
    assert_eq!(list.len(), 2, "the list is the list");
    assert_eq!(column_of(value), "age", "the value is the value");
}


/// The production path **fails closed** when the two elaboration paths
/// disagree, and says so.
///
/// `root_queries_checked` runs source elaboration and the evaluator over every
/// root definition and requires their trees to be equal. This pins what happens
/// when they are not, because that is a *policy* rather than an accident of the
/// code:
///
/// * the definition is reported as an **error**, so no `Rel` is emitted for it;
/// * the message is an *internal* error naming the definition, because a
///   disagreement means the compiler is wrong, not the program;
/// * the message says the definition **was not compiled**, which is what
///   fail-closed means. An earlier revision's wording claimed the evaluator's
///   result was still used, which contradicted the code — every disagreement
///   path returns `Err`.
///
/// Failing closed is temporary while source elaboration is incomplete, so this
/// test makes changing it a deliberate act: switching to "ship the evaluator's
/// tree and warn" has to update this test and cannot happen by quietly editing a
/// comment.
///
/// The program below is a genuine gap as of writing, not a hypothetical: a
/// window template (`sumOver`) taken as a bare value and applied later.
/// `CheckedValue::Callable::Template` cannot carry a window spec, because a spec
/// is a `WinSpecChecked` rather than an expression, so the deferred completion
/// reports it — while the evaluator builds the program fine. When that gap is
/// closed this test must be given a different program, which is the point.
#[test]
fn a_parity_gap_fails_closed_and_says_so() {
    let ws = Workspace::from_source(
        "ev : query { id = int, d = date } = table \"p\" \"ev\"\n\
         idn = f => f\n\
         w = idn (sumOver { partition = [.id] })\n\
         q = ev & select { n = w .id }\n",
    );
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = crate::check::check(&ws);

    // The evaluator alone builds it, which is what makes this a compiler gap
    // rather than a user error.
    let oracle = crate::eval::root_queries_via_evaluator(&ws, &tc);
    let (_, o) = oracle.iter().find(|(n, _)| n == "q").expect("`q`");
    assert!(
        o.is_ok(),
        "this test needs a program the evaluator *can* build, or it proves \
         nothing about the policy: {o:?}"
    );

    // Production fails closed: no relation for `q`, and a message that says so.
    let prod = crate::root_queries_checked(&ws, &tc);
    let (_, p) = prod.iter().find(|(n, _)| n == "q").expect("`q` reported");
    let d = p
        .as_ref()
        .expect_err("a parity gap must not emit a relation");
    assert!(
        d.message.contains("internal error"),
        "a compiler disagreement is an internal error, not a program error: {}",
        d.message
    );
    assert!(
        d.message.contains("was not compiled"),
        "the message must tell the user nothing was emitted, which is what \
         fail-closed means: {}",
        d.message
    );
    assert!(
        !d.message.contains("result was used"),
        "the message must not claim the evaluator's tree shipped, because it \
         did not: {}",
        d.message
    );

    // And the agreeing case still compiles, so failing closed has not been
    // implemented by failing always.
    let ok = Workspace::from_source(
        "ev : query { id = int, d = date } = table \"p\" \"ev\"\n\
         q = ev & select { n = rowNumber { order = [asc .id] } }\n",
    );
    let tc2 = crate::check::check(&ok);
    let prod2 = crate::root_queries_checked(&ok, &tc2);
    let (_, p2) = prod2.iter().find(|(n, _)| n == "q").expect("`q` reported");
    assert!(
        p2.is_ok(),
        "a program both paths handle must still compile: {:?}",
        p2.as_ref().err()
    );
}

/// An elaboration failure is told apart from a program error, and the
/// difference reaches the user.
///
/// Both cases arrive at the same place: source elaboration saw a query and could
/// not build one. They must not be reported the same way.
///
/// * **The program is wrong.** `q = table "s" "t"` declares no columns, so the
///   query is genuinely untypeable. The user must see that explanation, not an
///   internal error — and not a panic.
/// * **The compiler is limited.** A construct the evaluator handles and
///   `elaborate.rs` does not is a capability gap, reported as an internal error.
///
/// Getting this wrong is not cosmetic. Without the distinction, demoting
/// `schema` from a validator to an assertion made the compiler *panic* on the
/// first case, because the abandoned validator had been the only thing producing
/// that message. The fix was to classify the failure (`core::Fault`) rather than
/// to keep `schema` deciding.
#[test]
fn a_program_error_is_not_reported_as_a_compiler_gap() {
    // A program error: the table's columns are unknown, so nothing can be
    // checked. The message is the user's, and it must not be an internal error.
    let ws = Workspace::from_source(
        "users : query { id = int, age = int } = table \"p\" \"users\"\n\
         q = table \"s\" \"t\"\n",
    );
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    let tc = crate::check::check(&ws);
    let out = crate::root_queries_checked(&ws, &tc);
    let (_, q) = out.iter().find(|(n, _)| n == "q").expect("`q` reported");
    let d = q
        .as_ref()
        .expect_err("a column-less table cannot be compiled");
    assert!(
        d.message.contains("columns of table") && d.message.contains("unknown"),
        "the user must get the column explanation: {}",
        d.message
    );
    assert!(
        !d.message.contains("internal error"),
        "a malformed program is not a compiler bug: {}",
        d.message
    );

    // The classification itself, so a future edit to either constructor is
    // caught here rather than in a panic at the production boundary.
    let elab = crate::elaborate::elaborate_module(&ws, &tc, ws.root);
    let (_, r) = elab.iter().find(|(n, _)| n == "q").expect("`q` walked");
    let e = r.as_ref().expect_err("the elaborator cannot build it");
    assert!(
        !e.is_unsupported(),
        "a column-less table is the program's fault, not this phase's: {}",
        e.message
    );
}

/// The elaborated path's verdict is what reaches the user, even for a program
/// the *evaluator* also rejects.
///
/// This is the routing gap the earlier `assert!`-based test could not see. That
/// test checked the elaborator's classification in isolation, and its production
/// assertion passed on the `schema` diagnostic alone — which was the point: for
/// this program `schema_located` produces the identical message, so the
/// assertion could not tell which path had spoken.
///
/// The routing used to return the evaluator's error *before* consulting the
/// elaborated path, so for any program both paths reject, `schema` decided while
/// the surrounding code claimed the elaborated path did. Now both outcomes are
/// computed and reconciled first, which this test distinguishes by construction:
/// it asserts that the elaborated path reached its own classification, and that
/// the message the user gets is that one.
#[test]
fn the_elaborated_verdict_wins_over_a_schema_error() {
    let ws = Workspace::from_source("q = table \"s\" \"t\"\n");
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = crate::check::check(&ws);

    // The evaluator alone rejects this too — so the old routing short-circuited
    // here and never looked at the elaborated path.
    let oracle = crate::eval::root_queries_via_evaluator(&ws, &tc);
    let (_, o) = oracle.iter().find(|(n, _)| n == "q").expect("`q`");
    assert!(
        o.is_err(),
        "this test needs a program the evaluator *also* rejects, or it does not \
         exercise the routing gap: {o:?}"
    );

    // Source elaboration classifies it as a program error, not a gap.
    let elab = crate::elaborate::elaborate_module(&ws, &tc, ws.root);
    let (_, r) = elab.iter().find(|(n, _)| n == "q").expect("`q` walked");
    let e = r.as_ref().expect_err("a column-less table cannot be built");
    assert!(!crate::elaborate::is_not_a_query(e), "it is a query definition");
    assert!(
        !e.is_unsupported(),
        "a column-less table is the program's fault: {}",
        e.message
    );

    // The user gets the elaborated path's verdict: a plain program error, not
    // an internal error.
    //
    // **On the message alone this test could not tell which path spoke.** For
    // this program `schema_located` produces the *identical* string — the two
    // were deliberately written to share wording so a user sees one
    // explanation — so asserting on the text passes whichever path decided.
    // That is precisely the limitation the earlier test had.
    //
    // What distinguishes them is the **location**. The elaborated path reports
    // at its own `Origin` (the `table` primitive, column 5); `schema_located`
    // blames the `Rel` node it walked, which for this program is the whole
    // definition. So the span is the observable, and it is what is asserted.
    let out = crate::root_queries_checked(&ws, &tc);
    let (_, q) = out.iter().find(|(n, _)| n == "q").expect("`q` reported");
    let d = q.as_ref().expect_err("it cannot compile");
    assert!(
        !d.message.contains("internal error"),
        "a malformed program is not a compiler bug: {}",
        d.message
    );
    // The elaborated error's origin is the `table` primitive; `schema_located`
    // would blame the definition. Columns differ, so this distinguishes them.
    let origin = e.origin.expect("the elaborated error carries an origin");
    let start = origin.span.start as usize;
    let text = &ws.modules[origin.module].text;
    let origin_col = text[..start.min(text.len())]
        .rfind('\n')
        .map(|i| start - i)
        .unwrap_or(start + 1);
    assert_eq!(
        d.col, origin_col,
        "the diagnostic must come from the elaborated path at its own origin, not \
         from schema at the definition: {} at col {}, elaborated origin at col {}",
        d.message, d.col, origin_col
    );
}

/// The reconciliation table, asserted **cell by cell**.
///
/// This tests the decision rather than its output, and that is not a stylistic
/// choice. For the programs that motivated the routing work, `schema` and the
/// checked constructors produce the *identical message and column*, so no
/// assertion on a diagnostic can tell which path spoke — disabling the
/// elaborated path entirely leaves those outputs unchanged, verified by doing
/// it. The policy therefore has to be pinned where it lives.
///
/// The matrix is `Outcome` (source elaboration) against `Evaluated` (the
/// evaluator). `Option` is part of the answer: `None` means *no relation is
/// emitted*, which is the correct outcome for a definition that is not a query
/// rather than an error.
///
/// Every cell is covered, including ones not currently reachable, so reordering
/// or dropping a case fails here instead of silently changing who decides.
#[test]
fn the_reconciliation_table_is_the_policy() {
    use crate::eval::{reconcile, Evaluated, Outcome};

    // One real program, so the agreeing cell exercises the real erasure and the
    // real comparison rather than two unrelated stand-ins.
    let ws = Workspace::from_source("q : query { a = int } = table \"p\" \"t\"\n");
    let tc = crate::check::check(&ws);
    let Evaluated::Query(Ok(rel)) = crate::eval::evaluate_root(&ws, &tc)
        .into_iter()
        .find(|(n, _)| n == "q")
        .expect("`q` evaluated")
        .1
    else {
        panic!("`q` must be a query the evaluator builds");
    };
    // `ws.root`, not module 0: the prelude is module 0, so hardcoding it built
    // diagnostics out of the prelude's `_&_` span instead of `q`'s.
    let root = ws.root;
    let def_span = ws.modules[root].module.defs[0].span;
    let diag = |m: &str| ws.diag_span(root, def_span, m);
    let ok = || Evaluated::Query(Ok(rel.clone()));
    let eval_failed = || Evaluated::Failed(diag("evaluator said no"));
    let eval_rejected = || Evaluated::Rejected(diag("checker said no"));

    // Helper: is this cell an internal (compiler) error rather than a program
    // error or a success?
    let is_internal = |r: &Option<Result<Rel, Diag>>| {
        matches!(r, Some(Err(d)) if d.message.contains("internal error"))
    };

    // ── (1) ProgramError from source elaboration wins over anything ──────────
    for eval in [ok(), eval_failed(), eval_rejected(), Evaluated::Open] {
        let r = reconcile(&ws, "q", &Outcome::ProgramError("elaborated said no", None), eval);
        assert_eq!(
            r.expect("a program error still yields a diagnostic").expect_err("cannot compile").message,
            "elaborated said no",
            "the elaborated path's verdict must be the user's message"
        );
    }

    // ── (2) Unsupported fails closed, whatever the evaluator did ─────────────
    for eval in [ok(), eval_failed(), Evaluated::Rejected(diag("x")), Evaluated::NotQuery("a lambda".into()), Evaluated::Open] {
        let r = reconcile(&ws, "q", &Outcome::Unsupported("no can do"), eval);
        assert!(
            is_internal(&r),
            "a capability gap must fail closed as an internal error, got {r:?}"
        );
    }

    // ── (3) NotQuery defers entirely to the evaluator ────────────────────────
    //     A definition that is not a query is not compiled as one, so `None`
    //     (no relation) is the answer, not an error.
    assert!(
        reconcile(&ws, "q", &Outcome::NotQuery, Evaluated::NotQuery("an int".into())).is_none(),
        "a scalar definition emits nothing and is not an error"
    );
    assert!(
        reconcile(&ws, "q", &Outcome::NotQuery, Evaluated::Open).is_none(),
        "an overloaded helper emits nothing"
    );
    assert!(
        matches!(reconcile(&ws, "q", &Outcome::NotQuery, ok()), Some(Ok(_))),
        "an evaluator query stands when the elaborator saw none"
    );
    for eval in [eval_failed(), eval_rejected()] {
        let r = reconcile(&ws, "q", &Outcome::NotQuery, eval);
        assert!(
            matches!(&r, Some(Err(d)) if !d.message.contains("internal error")),
            "the evaluator's own failure is a user diagnostic, not an internal error: {r:?}"
        );
    }

    // ── (4) Built: only agreement ships ──────────────────────────────────────
    //     This is the cell the review found missing. A built query plus an
    //     evaluator failure is a *disagreement*, not a program error: one path
    //     says the program is fine, so presenting the evaluator's diagnostic
    //     would blame the user for a compiler disagreement.
    assert!(
        is_internal(&reconcile(&ws, "q", &built(&ws), eval_failed())),
        "Built + evaluator failure must be an internal disagreement"
    );
    assert!(
        is_internal(&reconcile(&ws, "q", &built(&ws), eval_rejected())),
        "Built + checker rejection must be an internal disagreement"
    );
    //     The omission hole: a definition the evaluator evaluated to a non-query
    //     while the elaborator built a query. Under the old shape this case had
    //     no entry at all, so nothing was reported.
    assert!(
        is_internal(&reconcile(&ws, "q", &built(&ws), Evaluated::NotQuery("a lambda".into()))),
        "Built + evaluator NotQuery is the omission hole and must be reported"
    );
    assert!(
        is_internal(&reconcile(&ws, "q", &built(&ws), Evaluated::Query(Err(diag("bad tree"))))),
        "Built + evaluator tree that fails validation is a disagreement"
    );
    assert!(
        reconcile(&ws, "q", &built(&ws), Evaluated::Open).is_none(),
        "an overloaded helper has no body to compare"
    );
    assert!(
        matches!(reconcile(&ws, "q", &built(&ws), ok()), Some(Ok(_))),
        "two agreeing queries ship the relation"
    );
}

/// The elaborator side of the matrix: the `CheckedQuery` for the same program.
fn built(ws: &Workspace) -> crate::eval::Outcome<'static> {
    // Built by the real constructors, so the agreeing cell exercises the real
    // erasure and comparison. Leaked deliberately: this lives for one test, and
    // the alternative is threading a lifetime through every call above.
    let tc = crate::check::check(ws);
    let elab = crate::elaborate::elaborate_module(ws, &tc, ws.root);
    let q = elab
        .into_iter()
        .find_map(|(n, r)| if n == "q" { r.ok() } else { None })
        .expect("`q` elaborates");
    crate::eval::Outcome::Built(Box::leak(Box::new(q)))
}

/// No definition can be silently omitted from the parity check.
///
/// The review's second finding, and the sharper of the two. `root_queries_
/// via_evaluator` used to drop an `Ok(Value)` that was not a query, and the
/// reconciler iterates only what it returns — so a definition the evaluator
/// evaluated to a scalar or a lambda had **no entry** and was never compared.
/// A definition the elaborator built as a query could therefore go unchecked and
/// vanish from the output with nothing reported.
///
/// `h = x => x` is the concrete case: a lambda, evaluated to a closure, dropped.
///
/// The fix is that the evaluator now reports an outcome for every definition
/// (`Evaluated`), so absence is no longer the way "not a query" is expressed.
/// What this test pins is the *property*, not the mechanism: every definition
/// the elaborator walks appears in the evaluator's outcome list, so no
/// definition can be skipped by the reconciler.
#[test]
fn every_definition_reaches_reconciliation() {
    let src = "t : query { a = int, b = string } = table \"p\" \"t\"\n\
               h = x => x\n\
               n = 1\n\
               q = t & select { a = h .a }\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    let root = ws.root;

    let evaluated: Vec<String> = crate::eval::evaluate_root(&ws, &tc)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let walked = crate::elaborate::elaborate_module(&ws, &tc, root);

    // Every definition of the user's module is reported by the evaluator. This
    // is what makes the omission hole impossible: the reconciler iterates this
    // list, so a missing entry is a missing comparison.
    for d in &ws.modules[root].module.defs {
        assert!(
            evaluated.contains(&d.name),
            "`{}` was not evaluated, so the reconciler never sees it — this is the \
             omission hole. Evaluated: {evaluated:?}",
            d.name
        );
    }

    // And the elaborator walked all of them too, so the two lists line up.
    assert_eq!(
        walked.len(),
        ws.modules[root].module.defs.len(),
        "the elaborator must report one outcome per definition"
    );

    // `h` is the specific case that used to vanish: it is a lambda, so the
    // evaluator produces a non-query value for it.
    let h = crate::eval::evaluate_root(&ws, &tc)
        .into_iter()
        .find(|(n, _)| n == "h")
        .expect("`h` is evaluated")
        .1;
    assert!(
        matches!(h, crate::eval::Evaluated::NotQuery(_)),
        "`h` evaluates to a closure, which is now *reported* rather than dropped"
    );
}

/// The invariant that gates making the evaluator test-only.
///
/// Only one cell of the reconciliation table still ships an evaluator-produced
/// `Rel`: cell (3), where the elaborator reports `NotAQuery` and the evaluator
/// built a query. Everything else ships the checked erasure.
///
/// That cell has a precondition, and this test states it: **a definition the
/// checker accepts as a query is built by the elaborator.** If the elaborator
/// says `NotAQuery`, the evaluator either failed or produced a non-query — never
/// a query.
///
/// Two consequences, both worth having:
///
/// * cell (3)'s `Query(Ok)` branch is currently **unreachable for accepted
///   programs**, so the evaluator is not deciding output in practice. That is
///   the precondition for making it test-only, and it is now asserted rather
///   than believed.
/// * if the elaborator ever stops building something it used to build, a
///   definition moves into cell (3) and the evaluator silently resumes being a
///   producer. That is exactly the regression this catches.
///
/// The corpus is every definition of the examples plus a set of type shapes
/// chosen to reach the boundary — a query-typed definition whose body is a
/// query, a scalar-typed one whose body is a query, a row-typed one, a helper
/// left open, and an overload set.
#[test]
fn an_accepted_query_is_never_only_the_evaluator_s() {
    let mut sources: Vec<String> = Vec::new();
    for f in std::fs::read_dir("../../examples").into_iter().flatten().flatten() {
        let p = f.path();
        if p.extension().and_then(|e| e.to_str()) == Some("cagara") {
            if let Ok(src) = std::fs::read_to_string(&p) {
                sources.push(src);
            }
        }
    }
    assert!(
        sources.len() >= 4,
        "the example corpus is missing; this test would pass vacuously \
         with {} sources",
        sources.len()
    );
    // Shapes chosen to reach the boundary, including ones the checker rejects:
    // the point is to cover the *classification*, not to compile.
    for src in [
        "t : query { a = int } = table \"s\" \"t\"\nq : query { a = int } = t\n",
        "t : query { a = int } = table \"s\" \"t\"\nq : int = t\n",
        "t : query { a = int } = table \"s\" \"t\"\nr : { a = int } = { a = 1 }\nq = t\n",
        "t : query { a = int } = table \"s\" \"t\"\nh = x => t\nq = t\n",
        "t : query { a = int } = table \"s\" \"t\"\nf : int -> int = x => x + 1\nq = t\n",
    ] {
        sources.push(src.to_string());
    }

    let mut checked = 0usize;
    for src in &sources {
        let ws = Workspace::from_source(src);
        let tc = crate::check::check(&ws);
        let walked = crate::elaborate::elaborate_module(&ws, &tc, ws.root);
        for (name, e) in crate::eval::evaluate_root(&ws, &tc) {
            // Only definitions the evaluator builds as a query could be produced
            // by cell (3).
            if !matches!(e, crate::eval::Evaluated::Query(Ok(_))) {
                continue;
            }
            checked += 1;
            let built = walked.iter().any(|(n, r)| n == &name && r.is_ok());
            let not_query = walked.iter().any(|(n, r)| {
                n == &name && r.as_ref().err().is_some_and(crate::elaborate::is_not_a_query)
            });
            assert!(
                built,
                "`{name}`: the evaluator built a query and the elaborator reported \
                 `NotAQuery`, so this definition takes cell (3) and the evaluator is \
                 deciding output. Either the elaborator regressed, or cell (3) is \
                 genuinely live and the evaluator cannot become test-only yet."
            );
            assert!(
                !not_query,
                "`{name}`: the elaborator reported `NotAQuery` while the evaluator \
                 built a query — cell (3), reached"
            );
        }
    }
    assert!(
        checked > 0,
        "no evaluator-built query definitions were found, so this proves nothing"
    );
}
