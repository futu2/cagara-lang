//! Compile-time benchmarks for the type checker and the rest of the pipeline.
//!
//! These are not micro-benchmarks; they compile representative programs of
//! growing size and assert a generous upper bound on the wall time. The bound
//! is wide enough to survive a slow or loaded CI machine, but tight enough to
//! catch an accidental exponential blow-up — the checker's overload fitting is
//! cached, and a regression that defeats the cache shows up here as a factor,
//! not as noise.
//!
//! Timings are printed (`--nocapture`) so a change is visible in a log even
//! when it stays under the bound.
//!
//! The programs are built from the same shapes a user writes: a long pipeline,
//! a long operator chain, and a wide record. `the_pipeline_is_not_exponential`
//! additionally checks the *scaling* rather than only the absolute time, since
//! a quadratic checker can still pass a one-size bound on a fast machine.

use cagara_hir::{check, root_queries_checked, Workspace};
use cagara_sql::{compile, Options};
use std::time::{Duration, Instant};

const SCHEMA: &str = "\
users : query { id = int, name = string, age = int, active = bool } = table \"public\" \"users\"\n";

/// A pipeline of `n` stages over `users`, each one a plain filter or
/// projection so the whole chain type-checks. `update` is what keeps the rest
/// of the row; `select` replaces it.
fn pipeline(n: usize) -> String {
    let mut q = String::from("users");
    for i in 0..n {
        q.push_str(&match i % 3 {
            0 => format!(" & where (.age > {i})"),
            1 => format!(" & update {{ c{i} = .age + {i} }}"),
            _ => format!(" & update {{ age = .age + {i} }}"),
        });
    }
    format!("q = {q}\n")
}

/// One expression built from `n` left-associative `+` operators.
fn chain(n: usize) -> String {
    let e = vec![".age"; n + 1].join(" + ");
    format!("q = users & select {{ x = {e} }}\n")
}

/// A record with `n` fields, built in one `select`.
fn wide(n: usize) -> String {
    let fs: Vec<String> = (0..n).map(|i| format!("f{i} = .age + {i}")).collect();
    format!("q = users & select {{ {} }}\n", fs.join(", "))
}

/// `n` independent definitions, each with a stage, so name resolution and the
/// per-definition bookkeeping are exercised too.
fn many_defs(n: usize) -> String {
    let mut out = String::new();
    for i in 0..n {
        out.push_str(&format!("d{i} = users & where (.age > {i})\n"));
    }
    out.push_str("q = users & select { x = .id }\n");
    out
}

/// Time the checker over `src`, returning the elapsed time and the number of
/// errors (which must be zero: a benchmark that measures an error path is
/// measuring the wrong thing).
fn time_check(src: &str) -> (Duration, usize) {
    let full = format!("{SCHEMA}{src}");
    let ws = Workspace::from_source(&full);
    assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);
    let t = Instant::now();
    let tc = check(&ws);
    let d = t.elapsed();
    assert!(
        tc.errors.is_empty(),
        "benchmark program has type errors: {:?}",
        tc.errors
            .iter()
            .map(|e| &e.diag.message)
            .collect::<Vec<_>>()
    );
    (d, tc.errors.len())
}

/// Time the checker, then lower the result to SQL, so a benchmark covers the
/// whole compile rather than only the checker.
fn time_compile(src: &str) -> Duration {
    let full = format!("{SCHEMA}{src}");
    let ws = Workspace::from_source(&full);
    let t = Instant::now();
    let tc = check(&ws);
    for (_, r) in root_queries_checked(&ws, &tc) {
        if let Ok(rel) = r {
            let _ = compile(&rel, Options::default());
        }
    }
    t.elapsed()
}

/// Generous ceiling for one measurement. These programs take well under a
/// second on a laptop; 10 s is slow enough for a loaded CI runner and still
/// catches a quadratic jump at the larger sizes.
const LIMIT: Duration = Duration::from_secs(10);

#[track_caller]
fn assert_under(label: &str, d: Duration) {
    println!("{label}: {d:?}");
    assert!(d < LIMIT, "{label} took {d:?}, over the {LIMIT:?} budget");
}

#[test]
fn a_long_pipeline_checks_in_reasonable_time() {
    // Deliberately modest. A pipeline is a chain of relational nodes, and the
    // checker and the SQL lowerer descend it recursively, so the depth that
    // fits depends on the thread's stack: the binary's main thread (8 MiB)
    // takes far more stages than a spawned test thread (2 MiB, the same
    // default the language server runs on). The parser and lowerer share a
    // defensive expression budget rather than imposing a ten-stage language
    // limit.
    for n in [2usize, 5, 9, 20] {
        let src = pipeline(n);
        let (d, _) = time_check(&src);
        assert_under(&format!("pipeline {n} stages"), d);
        assert_under(&format!("pipeline {n} stages (to SQL)"), time_compile(&src));
    }
}

#[test]
fn a_long_operator_chain_checks_in_reasonable_time() {
    // Stays under the parser's chain budget (192 in `cagara-syntax`), since a
    // longer one is a syntax error rather than a benchmark.
    for n in [10usize, 40, 80, 120] {
        let (d, _) = time_check(&chain(n));
        assert_under(&format!("chain {n} operators"), d);
    }
}

#[test]
fn a_wide_record_checks_in_reasonable_time() {
    for n in [10usize, 40, 80] {
        let (d, _) = time_check(&wide(n));
        assert_under(&format!("record {n} fields"), d);
    }
}

#[test]
fn many_definitions_check_in_reasonable_time() {
    for n in [10usize, 50, 100] {
        let (d, _) = time_check(&many_defs(n));
        assert_under(&format!("{n} definitions"), d);
    }
}

/// Scaling, not just absolute time. A checker that is accidentally exponential
/// blows up here even on a machine fast enough to hide it in the single-size
/// tests above.
///
/// The growth is measured over a small and a large chain. A roughly quadratic
/// checker is acceptable for these sizes (the checker is not the bottleneck at
/// the scales a person writes), so the bound is generous: 10x the operators
/// must not cost more than 40x the time. Exponential growth would be far past
/// that.
#[test]
fn the_pipeline_is_not_exponential() {
    let small = 10usize;
    let large = 100usize;
    // Warm up, so first-call allocation is not charged to the small case.
    let _ = time_check(&chain(5));

    // Take the best of a few runs: the smallest time is the one least
    // disturbed by scheduling noise, which matters for a ratio.
    let best = |n: usize| {
        (0..3)
            .map(|_| time_check(&chain(n)).0)
            .min()
            .expect("at least one run")
    };
    let a = best(small);
    let b = best(large);
    println!("chain {small} -> {large}: {a:?} -> {b:?}");
    // Guard against a divide by (almost) zero on a very fast machine.
    let ratio = b.as_secs_f64() / a.as_secs_f64().max(1e-6);
    assert!(
        ratio <= 40.0,
        "10x the operators cost {ratio:.1}x the time ({a:?} -> {b:?}); \
         the checker may have gone exponential"
    );
}
