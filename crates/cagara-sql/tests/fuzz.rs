//! Property test: a *valid* Cagara program never panics the compiler.
//!
//! `cagara-fmt` already fuzzes the parser with token soup, which is mostly
//! rejected. This generates programs that are meant to type-check — real
//! tables, real stages, real operators — and runs each one through the whole
//! pipeline: parse, type check, evaluate to the IR, validate its schema, and
//! lower to SQL. A program that panics (rather than reporting a diagnostic) is
//! a bug whether or not it happens to be well-typed, so an assertion here is
//! only about *not crashing*; a legitimate type error is an acceptable
//! outcome.
//!
//! The generator tracks the columns each stage produces, so the programs it
//! builds really do compile. `the_generator_reaches_sql` pins that floor: a
//! fuzzer whose programs are all rejected tests nothing.
//!
//! The generator is deterministic (seeded xorshift), so a failure prints the
//! seed and the program.

use cagara_hir::{check, root_queries_checked, Workspace};
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
            let f = rng.pick(&["rowNumber", "rank", "denseRank", "countOver"]);
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
fn check_pipeline(src: &str, seed: u64) {
    let full = format!("{SCHEMA}{src}");
    // 1. Parse and load. This must never panic; loading errors are fine.
    let ws = Workspace::from_source(&full);
    // 2. Type check, as the language server does.
    let tc = check(&ws);
    // 3. Evaluate to the IR and validate its schema, then lower to SQL for
    //    every dialect we claim to support.
    for (_, result) in root_queries_checked(&ws, &tc) {
        let Ok(rel) = result else { continue };
        for d in ["ansi", "postgres", "sqlite", "duckdb", "mysql", "tsql"] {
            let dialect =
                Dialect::from_str(d).unwrap_or_else(|| panic!("seed {seed}: unknown dialect {d}"));
            let opts = Options {
                dialect,
                pretty: false,
                optimize: false,
            };
            // A lowering error is a legitimate outcome; a panic is not.
            let _ = compile(&rel, opts);
        }
    }
}

/// Compile the generated query to ANSI SQL, if it type-checks and evaluates.
/// The generator names its definitions `q0`, `q1`, ...; take the first.
fn compile_one(src: &str) -> Option<String> {
    let ws = Workspace::from_source(&format!("{SCHEMA}{src}"));
    let tc = check(&ws);
    let (_, result) = root_queries_checked(&ws, &tc).into_iter().next()?;
    let rel = result.ok()?;
    compile(&rel, Options::default()).ok()
}

#[test]
fn generated_valid_programs_never_panic_the_compiler() {
    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let n = 1 + rng.below(3);
        let mut src = String::new();
        for i in 0..n {
            src.push_str(&program(&mut rng, i));
        }
        check_pipeline(&src, seed);
    }
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
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            let src = format!("q = users & {a} & {b}\n");
            check_pipeline(&src, (i * 100 + j) as u64);
        }
    }
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
#[test]
fn the_generator_reaches_sql() {
    let mut compiled = 0;
    let mut total = 0;
    let mut examples = Vec::new();
    for seed in 1..=200u64 {
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
        "only {compiled}/{total} generated programs reached SQL; examples that did not:\n{}",
        examples.join("")
    );
}
