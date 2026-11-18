//! Property test: a *valid* Cagara program never panics the compiler, and
//! never silently disappears between checking and lowering.
//!
//! `cagara-fmt` already fuzzes the parser with token soup, which is mostly
//! rejected. This generates programs that are meant to type-check — real
//! tables, real stages, real operators — and runs each one through the whole
//! pipeline: parse, type check, normalize to the IR, validate its schema, and
//! lower to SQL. A program that panics (rather than reporting a diagnostic) is
//! a bug whether or not it happens to be well-typed.
//!
//! Not crashing is the weaker of the two properties this asserts. The stronger
//! one is **totality**: a definition the checker accepted must not vanish
//! silently. Skipping every `Err` — which is what this test used to do — hides
//! precisely the bug class the compiler kept producing, because a
//! checker-accepted definition that a later phase rejects looks exactly like a
//! definition that was never a query. So the loop below distinguishes the
//! three verdicts a definition can have (`Fault::{Program, Compiler,
//! NotAQuery}`) instead of collapsing them.
//!
//! The generator tracks the columns each stage produces, so the programs it
//! builds really do compile. `the_generator_reaches_sql` pins that floor: a
//! fuzzer whose programs are all rejected tests nothing.
//!
//! The generator is deterministic (seeded xorshift), so a failure prints the
//! seed and the program.

use cagara_hir::{check, root_queries_checked, root_verdicts, Fault, Workspace};
use cagara_sql::{compile, Dialect, Options};

/// A small deterministic PRNG (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

/// The tables every generated program starts from.
const SCHEMA: &str = "\
users : query { id = int, name = string, age = int, active = bool, born = date } = table \"public\" \"users\"\n\
orders : query { id = int, user_id = int, amount = float, status = string, created_at = date } = table \"public\" \"orders\"\n";

/// What kind of value a column holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Float,
    Str,
    Bool,
    Date,
}

/// The columns a pipeline currently has, in order.
#[derive(Clone, Debug)]
struct Row(Vec<(String, Ty)>);

impl Row {
    fn users() -> Row {
        Row(vec![
            ("id".into(), Ty::Int),
            ("name".into(), Ty::Str),
            ("age".into(), Ty::Int),
            ("active".into(), Ty::Bool),
            ("born".into(), Ty::Date),
        ])
    }

    fn orders() -> Row {
        Row(vec![
            ("id".into(), Ty::Int),
            ("user_id".into(), Ty::Int),
            ("amount".into(), Ty::Float),
            ("status".into(), Ty::Str),
            ("created_at".into(), Ty::Date),
        ])
    }

    fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn cols_of(&self, t: Ty) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, k)| *k == t)
            .map(|(n, _)| n.as_str())
            .collect()
    }

    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(n, _)| n == name)
    }

    fn ty_of(&self, name: &str) -> Ty {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| *t)
            .unwrap_or(Ty::Int)
    }
}

/// A predicate over the current row, using only columns that exist.
fn predicate(rng: &mut Rng, row: &Row) -> String {
    let mut opts: Vec<String> = Vec::new();
    if let Some(c) = row.cols_of(Ty::Int).first() {
        opts.push(format!(".{c} > {}", rng.below(50)));
        opts.push(format!(".{c} >= {}", rng.below(50)));
        opts.push(format!(".{c} <= {}", rng.below(50)));
        opts.push(format!("inList [1, 2] .{c}"));
        opts.push(format!(".{c} + 1 < 10"));
        opts.push(format!(".{c} != 0"));
    }
    if let Some(c) = row.cols_of(Ty::Str).first() {
        opts.push(format!(".{c} == \"x\""));
        opts.push(format!(".{c} <> \"y\""));
        opts.push(format!("contains \"a\" .{c}"));
        opts.push(format!("isNull (just .{c})"));
        opts.push(format!("isNotNull (just .{c})"));
    }
    if let Some(c) = row.cols_of(Ty::Bool).first() {
        opts.push(format!(".{c}"));
        opts.push(format!("not (.{c})"));
    }
    if let Some(c) = row.cols_of(Ty::Date).first() {
        opts.push(format!("year .{c} >= 2000"));
    }
    if opts.len() >= 2 && rng.chance(30) {
        let a = opts[rng.below(opts.len())].clone();
        let b = opts[rng.below(opts.len())].clone();
        return format!("({a}) {} ({b})", if rng.chance(50) { "&&" } else { "||" });
    }
    // `opts` always has at least one entry: every generated row has an int.
    opts[rng.below(opts.len())].clone()
}

/// A scalar expression of type `ty` over the current row, if one can be built.
fn expr(rng: &mut Rng, row: &Row, ty: Ty) -> Option<String> {
    let mut opts: Vec<String> = Vec::new();
    match ty {
        Ty::Int => {
            if let Some(c) = row.cols_of(Ty::Int).first() {
                opts.push(format!(".{c}"));
                opts.push(format!(".{c} + 1"));
                opts.push(format!(".{c} * 2"));
                opts.push(format!(".{c} - 1"));
                opts.push(format!(".{c} / 2"));
            }
            if let Some(c) = row.cols_of(Ty::Str).first() {
                opts.push(format!("length .{c}"));
            }
            if let Some(c) = row.cols_of(Ty::Date).first() {
                opts.push(format!("year .{c}"));
            }
            opts.push("1".into());
            opts.push("42".into());
            opts.push("(1 + 2) * 3".into());
        }
        Ty::Str => {
            if let Some(c) = row.cols_of(Ty::Str).first() {
                opts.push(format!(".{c}"));
                opts.push(format!("upper .{c}"));
                opts.push(format!("substring 1 3 .{c}"));
            }
            opts.push("\"lit\"".into());
        }
        Ty::Float => {
            if let Some(c) = row.cols_of(Ty::Float).first() {
                opts.push(format!(".{c}"));
            } else if let Some(c) = row.cols_of(Ty::Int).first() {
                opts.push(format!(".{c}"));
            }
            opts.push("1.5".into());
        }
        Ty::Bool => {
            if let Some(c) = row.cols_of(Ty::Bool).first() {
                opts.push(format!(".{c}"));
            }
            if let Some(c) = row.cols_of(Ty::Int).first() {
                opts.push(format!(".{c} > 0"));
            }
            opts.push("true".into());
        }
        Ty::Date => {
            if let Some(c) = row.cols_of(Ty::Date).first() {
                opts.push(format!(".{c}"));
            }
            opts.push("currentDate".into());
        }
    }
    if opts.is_empty() {
        None
    } else {
        Some(opts[rng.below(opts.len())].clone())
    }
}

/// `asc .x` / `desc .x` over a column of the row.
fn sort_order(rng: &mut Rng, row: &Row) -> String {
    let names = row.names();
    let c = names[rng.below(names.len())];
    format!("{} .{c}", rng.pick(&["asc", "desc"]))
}

/// One stage, together with the row it produces. `None` when the stage does
/// not apply to this row.
fn stage(rng: &mut Rng, row: &Row) -> Option<(String, Row)> {
    let names = row.names();
    if names.is_empty() {
        return None;
    }
    let pick_col = |rng: &mut Rng| names[rng.below(names.len())].to_string();
    match rng.below(12) {
        0 => Some((format!("where ({})", predicate(rng, row)), row.clone())),
        1 => {
            // Project existing columns under new names.
            let n = 1 + rng.below(3);
            let picked: Vec<String> = (0..n).map(|_| pick_col(rng)).collect();
            let fs: Vec<String> = picked
                .iter()
                .enumerate()
                .map(|(i, c)| format!("c{i} = .{c}"))
                .collect();
            let out = Row(picked
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("c{i}"), row.ty_of(c)))
                .collect());
            Some((format!("select {{ {} }}", fs.join(", ")), out))
        }
        2 => {
            // Add one computed int column, keeping the rest. `select`
            // replaces the row, so keeping the rest needs `update`.
            let e = expr(rng, row, Ty::Int)?;
            let mut out = row.clone();
            out.0.push(("v".into(), Ty::Int));
            Some((format!("update {{ v = {e} }}"), out))
        }
        3 => {
            // `update` recomputes a column in place, so the shape is kept.
            let e = expr(rng, row, Ty::Int).unwrap_or_else(|| "1".into());
            let target = pick_col(rng);
            Some((format!("update {{ {target} = {e} }}"), row.clone()))
        }
        4 => {
            // `agg` over a group key: only the key and the aggregates survive.
            let key = pick_col(rng);
            let kt = row.ty_of(&key);
            Some((
                format!("agg {{ {key} = group .{key}, n = count }}"),
                Row(vec![(key, kt), ("n".into(), Ty::Int)]),
            ))
        }
        5 => {
            let key = pick_col(rng);
            let kt = row.ty_of(&key);
            let mut out = Row(vec![(key.clone(), kt), ("n".into(), Ty::Int)]);
            let agg = match row.cols_of(Ty::Int).first() {
                Some(c) => {
                    out.0.push(("s".into(), Ty::Int));
                    format!(", s = sum .{c}")
                }
                None => String::new(),
            };
            Some((
                format!("agg {{ {key} = group .{key}, n = count{agg} }}"),
                out,
            ))
        }
        6 => Some((format!("order [{}]", sort_order(rng, row)), row.clone())),
        7 => Some((format!("limit {}", 1 + rng.below(50)), row.clone())),
        8 => Some((format!("offset {}", rng.below(50)), row.clone())),
        9 => Some(("distinct".to_string(), row.clone())),
        10 => {
            // A window adds a column; it may not be used as a sort or
            // partition key, and `where` cannot see it.
            let spec = if row.has("active") && rng.chance(50) {
                format!(
                    "{{ partition = [.active], order = [{}] }}",
                    sort_order(rng, row)
                )
            } else {
                format!("{{ order = [{}] }}", sort_order(rng, row))
            };
            let f = rng.pick(&["rowNumber", "rank", "denseRank"]);
            let mut out = row.clone();
            out.0.push(("w".into(), Ty::Int));
            Some((format!("update {{ w = {f} {spec} }}"), out))
        }
        _ => {
            // A whole-row aggregate (no group key).
            let mut out = Row(vec![("n".into(), Ty::Int)]);
            let distinct = match row.cols_of(Ty::Int).first() {
                Some(_) if rng.chance(50) => {
                    out.0.push(("s".into(), Ty::Int));
                    ", s = countDistinct .id"
                }
                _ => "",
            };
            Some((format!("agg {{ n = count{distinct} }}"), out))
        }
    }
}

/// A whole pipeline: a table, then a random number of applicable stages.
fn program(rng: &mut Rng, i: usize) -> String {
    let orders = rng.chance(20);
    let mut row = if orders { Row::orders() } else { Row::users() };
    let base = if orders { "orders" } else { "users" };
    let mut q = base.to_string();
    for _ in 0..1 + rng.below(4) {
        if let Some((s, next)) = stage(rng, &row) {
            q.push_str(" & ");
            q.push_str(&s);
            row = next;
        }
    }
    // Sometimes join the other table, so join lowering is exercised.
    if base == "users" && rng.chance(25) {
        let kind = rng.pick(&["innerJoin", "leftJoin", "rightJoin"]);
        let cond = rng.pick(&[
            ".<id == .>user_id",
            ".<age >= .>amount",
            ".<name == .>status",
        ]);
        q = format!("({q}) & {kind} orders ({cond})");
    }
    format!("q{i} = {q}\n")
}

/// Run one program through the whole pipeline.
///
/// Two properties, and the second is the one that earns its keep:
///
/// 1. **No panic.** Any phase may legitimately report an error, but none may
///    crash on a program the generator built.
/// 2. **Totality.** A definition the checker accepted must not vanish. The
///    generator builds programs that are *meant* to type-check, so when the
///    checker takes one and a later phase rejects it, that is a compiler
///    capability gap — the exact class of bug this project kept shipping. The
///    old loop skipped every `Err`, which made such a gap indistinguishable
///    from a definition that was never a query.
fn check_pipeline(src: &str, seed: u64) {
    let full = format!("{SCHEMA}{src}");
    // 1. Parse and load. This must never panic; loading errors are fine.
    let ws = Workspace::from_source(&full);
    // 2. Type check, as the language server does.
    let tc = check(&ws);
    // 3. Evaluate to the IR and validate its schema, then lower to SQL for
    //    every dialect we claim to support.
    for (name, result, fault) in root_verdicts(&ws, &tc) {
        // The gate. `Fault::Program` is a real diagnostic (and, since the
        // generator aims at legal programs, also a signal — but a weak one,
        // because a generator limitation can produce a genuine type error).
        // `Fault::Compiler` is never legitimate: the checker accepted this
        // definition, so no later phase may refuse it.
        assert!(
            fault != Fault::Compiler,
            "seed {seed}: `{name}` was accepted by the checker but a later phase \
             reported a compiler gap\n{src}\n-> {:?}",
            result.as_ref().err().map(|d| &d.message)
        );
        let Ok(rel) = result else { continue };
        // Every dialect, and for each one the *outcome* is pinned rather than
        // discarded. Discarding it is what made this fuzzer blind to the bug class
        // finding #6 named: it detected a panic, but a dialect that wrongly accepted a
        // construct (SQLite emitting `ROLLUP`) or wrongly refused one looked exactly
        // like success.
        //
        // The outcome cannot be a fixed per-dialect answer — the generator emits
        // constructs some targets genuinely lack — so what is asserted is that the
        // answer is *stable and explainable*:
        let mut outcomes: Vec<(&str, Result<String, String>)> = Vec::new();
        for d in DIALECTS {
            let dialect =
                Dialect::from_str(d).unwrap_or_else(|| panic!("seed {seed}: unknown dialect {d}"));
            let opts = Options {
                dialect,
                pretty: false,
                optimize: false,
            };
            // A lowering error is a legitimate outcome; a panic is not.
            let got = compile(&rel, opts);
            if let Err(message) = &got {
                // An `internal:` failure is a compiler gap, never a refusal: the
                // checker accepted this program and every dialect refusal is written
                // as advice to the reader.
                assert!(
                    !message.starts_with("internal:"),
                    "seed {seed}: `{name}` / {d} reported an internal error\n{src}\n-> {message}"
                );
                // And a refusal has to name the target, so a reader can act on it.
                // ANSI is exempt: its refusals are about the relation, not the target.
                if *d != "ansi" {
                    assert!(
                        message.contains(d) || message.contains(&format!("`{d}`")),
                        "seed {seed}: `{name}` / {d} was refused without naming the target\
                         \n{src}\n-> {message}"
                    );
                }
            }
            // Determinism: lowering the same relation twice must give one answer.
            // A dialect pass that depended on iteration order or on a global would
            // show up here and nowhere else.
            let again = compile(&rel, opts);
            assert_eq!(
                got, again,
                "seed {seed}: `{name}` / {d} lowered differently on a second call"
            );
            outcomes.push((d, got));
        }
        // **ANSI is the most permissive target** — it has `FULL JOIN`, grouping sets,
        // all three frame units, arrays and an exact decimal, so it refuses nothing
        // about a spelling. A program that ANSI refuses is refused for a reason that
        // is not about the target, and every other target must refuse it too: a
        // construct cannot be dialect-independent in one direction only.
        if let Some((_, Err(why))) = outcomes.iter().find(|(d, r)| *d == "ansi" && r.is_err()) {
            for (d, got) in &outcomes {
                if *d == "ansi" {
                    continue;
                }
                assert!(
                    got.is_err(),
                    "seed {seed}: `{name}` was refused for ANSI ({why}) but accepted for {d}\
                     \n{src}"
                );
            }
        }
    }
}

/// The dialects a generated program is lowered for.
///
/// The six the project actually targets, including the two that emulate the most
/// (MySQL and SQLite) — a rewrite that only breaks on an emulating target would be
/// missed by a list of permissive ones.
const DIALECTS: &[&str] = &["ansi", "postgres", "sqlite", "duckdb", "mysql", "tsql"];

/// Compile the generated query to ANSI SQL, if it type-checks and normalizes.
/// The generator names its definitions `q0`, `q1`, ...; take the first.
fn compile_one(src: &str) -> Option<String> {
    let ws = Workspace::from_source(&format!("{SCHEMA}{src}"));
    let tc = check(&ws);
    let (_, result) = root_queries_checked(&ws, &tc).into_iter().next()?;
    let rel = result.ok()?;
    compile(&rel, Options::default()).ok()
}

/// Run seeds `lo..=hi` through the whole pipeline.
///
/// A range rather than a count so the split below can be a plain table: a failing
/// seed names itself in the panic either way, but the *test* that fails tells you
/// which tenth of the space to look in.
fn seeds_never_panic(lo: u64, hi: u64) {
    for seed in lo..=hi {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let n = 1 + rng.below(3);
        let mut src = String::new();
        for i in 0..n {
            src.push_str(&program(&mut rng, i));
        }
        check_pipeline(&src, seed);
    }
}

/// Four hundred generated programs, split across ten tests so they run at once.
///
/// This was one test, and one test is one thread: 68 seconds on a machine with
/// sixteen cores, for 400 independent compilations. libtest and nextest both
/// parallelise *across* tests and not inside one, so the seed range is the unit of
/// parallelism — the same reason `cagara-fmt`'s prefix tests are one per file.
///
/// The split is by seed, which is the only property that matters here: the
/// generator is seeded per program and `check_pipeline` holds no state between
/// them, so any partition of `1..=400` covers the same ground. Ten ranges of forty
/// keep the table readable and the failure attribution exact.
macro_rules! panic_free_tests {
    ($($name:ident: $lo:literal ..= $hi:literal,)*) => {
        $(
            #[test]
            fn $name() {
                seeds_never_panic($lo, $hi);
            }
        )*
    };
}

panic_free_tests! {
    generated_programs_1_to_40_never_panic_the_compiler: 1..=40,
    generated_programs_41_to_80_never_panic_the_compiler: 41..=80,
    generated_programs_81_to_120_never_panic_the_compiler: 81..=120,
    generated_programs_121_to_160_never_panic_the_compiler: 121..=160,
    generated_programs_161_to_200_never_panic_the_compiler: 161..=200,
    generated_programs_201_to_240_never_panic_the_compiler: 201..=240,
    generated_programs_241_to_280_never_panic_the_compiler: 241..=280,
    generated_programs_281_to_320_never_panic_the_compiler: 281..=320,
    generated_programs_321_to_360_never_panic_the_compiler: 321..=360,
    generated_programs_361_to_400_never_panic_the_compiler: 361..=400,
}

/// Every ordered pair of stages, so a stage that only lowers badly after a
/// particular predecessor is found deterministically rather than by luck.
#[test]
fn every_pair_of_stages_is_lowered() {
    let mut rng = Rng(7 | 1);
    let mut all: Vec<String> = Vec::new();
    for _ in 0..40 {
        if let Some((s, _)) = stage(&mut rng, &Row::users()) {
            all.push(s);
        }
    }
    assert!(
        all.len() >= 20,
        "the generator produced only {} stages",
        all.len()
    );
    // One thread per pair, because every pair is independent: the generators are
    // seeded per pair and `check_pipeline` reads no shared state. As a plain
    // double loop this was 400 compilations on one core — the single slowest test
    // in the workspace at 120 seconds, on a machine with sixteen.
    //
    // The work is split by *row* of the matrix rather than by pair, so the threads
    // are few (at most `all.len()`) and each one's failure still names the pair.
    let pairs: Vec<(usize, usize, String)> = all
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            all.iter()
                .enumerate()
                .map(move |(j, b)| (i, j, format!("q = users & {a} & {b}\n")))
        })
        .collect();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(pairs.len());
    let chunk = pairs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for slice in pairs.chunks(chunk) {
            scope.spawn(move || {
                for (i, j, src) in slice {
                    check_pipeline(src, (*i * 100 + *j) as u64);
                }
            });
        }
    });
}

/// The pipeline shorthands (`&?`, `&=`, `&*`, `&.`, `&-`) go through the same
/// primitives as the long forms, and must lower to the same SQL.
#[test]
fn pipeline_shorthands_match_their_long_forms() {
    let shorthands: &[(&str, &str)] = &[
        ("q = users &? .age > 1\n", "q = users & where (.age > 1)\n"),
        (
            "q = users &= { x = .id }\n",
            "q = users & select { x = .id }\n",
        ),
        (
            "q = users &* { n = count }\n",
            "q = users & agg { n = count }\n",
        ),
        ("q = users &. [asc .id]\n", "q = users & order [asc .id]\n"),
        ("q = users &- 5\n", "q = users & limit 5\n"),
    ];
    for (short, long) in shorthands {
        check_pipeline(short, 0);
        let a = compile_one(short).unwrap_or_else(|| panic!("`{short}` did not compile"));
        let b = compile_one(long).unwrap_or_else(|| panic!("`{long}` did not compile"));
        assert_eq!(a, b, "{short:?} and {long:?} differ");
    }
}

/// The generator must actually produce programs that reach SQL lowering; a
/// fuzzer that only ever produces type errors tests nothing. This pins a floor
/// on how many generated programs compile, so the generator cannot rot into
/// vacuity — an earlier version tracked no schema and compiled 0 of 200, and
/// a later one emitted `select { .., x = ... }`, which is not Cagara syntax at
/// all (a row spread is `update`).
///
/// The floor is high on purpose: every generated program should compile. If a
/// change to the generator drops this, the fuzzer has stopped covering the
/// lowering paths it exists to cover.
///
/// Split by seed range for the same reason as the panic tests: 200 compilations in
/// one test is 200 compilations on one thread, and this was the second-slowest test
/// in the workspace at 47 seconds. Each slice asserts its own floor, so a
/// regression still names the range it happened in.
fn seeds_all_reach_sql(lo: u64, hi: u64) {
    let mut compiled = 0;
    let mut total = 0;
    let mut examples = Vec::new();
    for seed in lo..=hi {
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let src = program(&mut rng, 0);
        total += 1;
        if compile_one(&src).is_some() {
            compiled += 1;
        } else if examples.len() < 3 {
            examples.push(src);
        }
    }
    assert_eq!(
        compiled,
        total,
        "only {compiled}/{total} generated programs reached SQL (seeds {lo}..={hi}); \
         examples that did not:\n{}",
        examples.join("")
    );
}

macro_rules! reaches_sql_tests {
    ($($name:ident: $lo:literal ..= $hi:literal,)*) => {
        $(
            #[test]
            fn $name() {
                seeds_all_reach_sql($lo, $hi);
            }
        )*
    };
}

reaches_sql_tests! {
    generated_programs_1_to_50_all_reach_sql: 1..=50,
    generated_programs_51_to_100_all_reach_sql: 51..=100,
    generated_programs_101_to_150_all_reach_sql: 101..=150,
    generated_programs_151_to_200_all_reach_sql: 151..=200,
}
