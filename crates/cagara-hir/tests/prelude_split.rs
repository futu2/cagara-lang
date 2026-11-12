//! The prelude is two files, and the split means something.
//!
//! `core.cagara` holds what the language itself needs — the pipeline, the
//! stages, the joins, the aggregates and windows, the operators, null handling.
//! `std.cagara` holds what a *query* reaches for — strings, math, casts, dates.
//! The checker sees one module (the two are concatenated), so this is a fact
//! about where a definition lives rather than about how it resolves; the point
//! of pinning it here is that the boundary decides what a minimal embedding
//! would have to keep.

use std::collections::BTreeSet;

fn read(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../prelude")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every name a file defines, in order of first appearance, overloads deduped.
fn names(src: &str) -> BTreeSet<String> {
    src.lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with(' ') && !l.starts_with('\t'))
        .filter_map(|l| l.split_once(':'))
        .map(|(n, _)| n.trim().to_string())
        .filter(|n| !n.is_empty() && !n.contains(' '))
        .collect()
}

#[test]
fn core_holds_the_language_and_std_holds_the_helpers() {
    let core = read("core.cagara");
    let std = read("std.cagara");

    let core_names = names(&core);
    let std_names = names(&std);
    assert!(!core_names.is_empty() && !std_names.is_empty());

    // The two files partition the prelude: a name is defined in exactly one.
    let both: Vec<&String> = core_names.intersection(&std_names).collect();
    assert!(both.is_empty(), "defined in both files: {both:?}");

    // What the language cannot do without is in core.
    for name in [
        "_&_",
        "table",
        "where",
        "select",
        "agg",
        "order",
        "innerJoin",
        "union",
        "group",
        "count",
        "sum",
        "rowNumber",
        "rows",
        "_==_",
        "_+_",
        "coalesce",
        "_??_",
        "just",
        "isNull",
    ] {
        assert!(core_names.contains(name), "`{name}` belongs in core");
    }

    // What a query reaches for is in std.
    for name in [
        "upper",
        "trim",
        "abs",
        "round",
        "toString",
        "toInt",
        "tryDate",
        "addDays",
        "truncMonth",
        "year",
        "daysBetween",
        "_++_",
        "concat",
        "eqMaybe",
        "nullIf",
        "countOfMaybe",
    ] {
        assert!(std_names.contains(name), "`{name}` belongs in std");
    }

    // Core is primitive-heavy, std is defined in the language: a `primitive`
    // in std would mean the compiler knows a name a library should own.
    assert!(
        !std.contains("primitive \""),
        "std must be written in the language, not in primitives"
    );
    assert!(
        core.matches("primitive \"").count() > 30,
        "core is where the primitives live"
    );
}

#[test]
fn the_prelude_the_checker_loads_is_both_files() {
    // `PRELUDE_SRC` is `core.cagara` then `std.cagara`; the order is what lets
    // std refer to core without a forward reference.
    let joined = cagara_hir::workspace::PRELUDE_SRC;
    let core = read("core.cagara");
    let std = read("std.cagara");
    assert!(
        joined.starts_with(core.trim_end()),
        "the prelude starts with core"
    );
    assert!(
        joined.trim_end().ends_with(std.trim_end()),
        "and ends with std"
    );
}
