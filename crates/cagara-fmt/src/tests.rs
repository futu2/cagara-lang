use super::*;

fn fmt(src: &str) -> String {
    let out = format(src).expect("formatter changed the program").text;
    let again = format(&out).expect("formatter changed its own output").text;
    assert_eq!(out, again, "not idempotent");
    out
}

#[track_caller]
fn check(src: &str, expected: &str) {
    assert_eq!(fmt(src), expected);
}

const SOURCES: &[(&str, &str)] = &[
    ("prelude", include_str!("../../../prelude.cagara")),
    ("report", include_str!("../../../examples/report.cagara")),
    ("public", include_str!("../../../examples/public.cagara")),
    ("schema", include_str!("../../../examples/schema.cagara")),
    ("errors", include_str!("../../../examples/errors.cagara")),
];

/// A definition the formatter cannot lay out is printed verbatim, so a
/// construct it does not understand used to pass every test above: the output
/// was stable, kept its comments, and simply never changed. This pins that
/// each construct is actually reformatted, by feeding it a deliberately
/// unformatted spelling and requiring a change.
#[test]
fn every_construct_is_actually_reformatted() {
    // A field shorthand is the case that regressed: `field` used to require
    // `name = value` and bail on `.name`, silently printing the whole
    // definition as written.
    for (src, want) in [
        ("q = t & select {.a, .b}\n", "q = t & select { .a, .b }\n"),
        (
            "q = t & select {.a, x = .b + 1}\n",
            "q = t & select { .a, x = .b + 1 }\n",
        ),
        (
            "t : query {a = int, b = string} = table \"s\" \"t\"\n",
            "t : query { a = int, b = string } = table \"s\" \"t\"\n",
        ),
        ("q=t&where(.a>1)\n", "q = t & where (.a > 1)\n"),
        ("q = t & update {.a}\n", "q = t & update { .a }\n"),
        // Operator declarations are items like any other, and a spelling the
        // prelude has not declared still parses as a declaration.
        (
            "infixl   1   &^\ninfixr   21   ~>\n",
            "infixl 1 &^\ninfixr 21 ~>\n",
        ),
    ] {
        assert_eq!(fmt(src), want, "input: {src:?}");
        assert_ne!(fmt(src), src, "not reformatted at all: {src:?}");
    }
    // The shorthand must survive in every position it is allowed in, and the
    // result must still parse to the same program (the `fmt` helper checks
    // idempotence, and `format` checks the token skeleton).
    for src in [
        "q = t & select {.a}\n",
        "q = t & select {.a, .b, .c}\n",
        "q = t & update {.a, .b}\n",
        "q = t & agg {k = group .k, n = count}\n",
        "q = t & select {\n  .a,\n  .b\n}\n",
    ] {
        let _ = fmt(src);
    }
}

#[test]
fn repo_sources_are_stable_and_keep_every_comment() {
    for (name, src) in SOURCES {
        let out = fmt(src);
        let comments = |s: &str| {
            s.lines()
                .filter(|l| l.trim_start().starts_with('#'))
                .count()
        };
        assert_eq!(
            comments(src),
            comments(&out),
            "{name}: comment lines changed"
        );
        assert!(
            out.lines()
                .all(|l| l.chars().count() <= WIDTH || !l.contains(' ')),
            "{name}: {out}"
        );
        // A definition printed verbatim comes back byte-identical, which the
        // checks above cannot see. Every shipped source is already formatted,
        // so a change here means the formatter learned (or forgot) something.
        assert_eq!(out, *src, "{name}: not in canonical form");
    }
}

/// Every prefix of a real file stands in for a file being typed: formatting
/// must never fail or change the program, and must reach a fixed point.
#[test]
fn every_prefix_of_the_examples_formats() {
    for (name, src) in SOURCES {
        for (i, _) in src.char_indices().step_by(37) {
            let prefix = &src[..i];
            let Ok(out) = format(prefix) else {
                panic!("{name} at {i}: FmtError on {prefix:?}")
            };
            assert_eq!(
                format(&out.text).map(|f| f.text),
                Ok(out.text.clone()),
                "{name} at {i}"
            );
        }
    }
}

#[test]
fn pipelines_break_per_stage() {
    check(
        "adults = users & where (.age >= 18) & select { id = .id }\n",
        "adults = users\n  & where (.age >= 18)\n  & select { id = .id }\n",
    );
    // One stage stays on one line.
    check(
        "n = users & agg { t = sum .age }\n",
        "n = users & agg { t = sum .age }\n",
    );
    // Continuation lines are re-indented.
    check(
        "a = q\n      & where .x\n        &- 10\n",
        "a = q\n  & where .x\n  &- 10\n",
    );
    // `&+` is a stage like any other, and is spaced like its siblings.
    check(
        "a = q &+{ age = .age + 1 } &= {.id}\n",
        "a = q\n  &+ { age = .age + 1 }\n  &= { .id }\n",
    );
}

#[test]
fn long_records_break_one_field_per_line() {
    let src = "s = q & select { id = .id, a_rather_long_name = upper .name, another_long_one = .age + 1, more_fields = .xyzw }\n";
    check(
        src,
        "s = q
  & select {
    id = .id,
    a_rather_long_name = upper .name,
    another_long_one = .age + 1,
    more_fields = .xyzw
  }
",
    );
}

#[test]
fn spacing_is_normalized() {
    check("x={a=1,b=[1,2]}\n", "x = { a = 1, b = [1, 2] }\n");
    check("f   =  x=>y   =>  x+y\n", "f = x => y => x + y\n");
    check(
        "t : expr r int->agg (expr  r int) = sql   \"SUM($1)\"\n",
        "t : expr r int -> agg (expr r int) = sql \"SUM($1)\"\n",
    );
    check(
        "r : query {id=int|r} = table \"t\"\n",
        "r : query { id = int | r } = table \"t\"\n",
    );
    check("e = {}\nl = []\n", "e = {}\nl = []\n");
}

#[test]
fn projection_and_application_keep_their_meaning() {
    check(
        "a = s.users\nb = f .x\nc = m.t.u\n",
        "a = s.users\nb = f .x\nc = m.t.u\n",
    );
    check("n = - .x\nm = 1 - -2\n", "n = -.x\nm = 1 - -2\n");
}

#[test]
fn comments_and_blank_lines() {
    check(
        "# header\n\n\n\na = 1 # trailing\n# doc for b\n\n\nb = 2\n\n# end\n",
        "# header\n\na = 1 # trailing\n# doc for b\n\nb = 2\n\n# end\n",
    );
    // Comments inside a pipeline force it to break and stay with their stage.
    check(
        "a = q & where .x # only x\n  & limit 1\n",
        "a = q\n  & where .x # only x\n  & limit 1\n",
    );
    check(
        "a = q\n  # keep active\n  & where .active\n",
        "a = q\n  # keep active\n  & where .active\n",
    );
    check(
        "r = {\n  a = 1, # one\n  b = 2\n}\n",
        "r = {\n  a = 1, # one\n  b = 2\n}\n",
    );
    check("x =\n  # why\n  1\n", "x =\n  # why\n  1\n");
    check("", "");
    check("# only a comment", "# only a comment\n");
}

#[test]
fn comments_never_push_a_name_into_column_zero() {
    // A name after an own-line comment inside a definition must stay indented.
    let out = fmt("a = f # c\n  x\nb : # t\n  int = 1\n");
    for line in out.lines().skip(1) {
        assert!(
            !line.starts_with(|c: char| c.is_alphabetic()) || line.starts_with("b "),
            "{out}"
        );
    }
}

#[test]
fn items_with_syntax_errors_are_left_as_written() {
    let r = format("good   =  1\nbad = (1 +\nalso_good={a=1}\n").unwrap();
    assert_eq!(r.text, "good = 1\nbad = (1 +\nalso_good = { a = 1 }\n");
    assert!(!r.errors.is_empty());
}

#[test]
fn long_arguments_break_onto_indented_lines() {
    let src = format!(
        "x = someFunction {} {} {}\n",
        "a".repeat(40),
        "b".repeat(40),
        "c".repeat(40)
    );
    check(
        &src,
        &format!(
            "x =\n  someFunction\n    {}\n    {}\n    {}\n",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40)
        ),
    );
}

#[test]
fn crlf_input() {
    check("a = 1\r\nb = 2\r\n", "a = 1\nb = 2\n");
}

#[test]
fn line_col_counts_chars() {
    assert_eq!(line_col("ab\ncé d", 7), (2, 4));
}

#[test]
fn types_break_at_the_width() {
    assert_eq!(format_type("f", "int -> int", 80), "f : int -> int");
    let t = "{ n = string, m = int } -> query { id = int, name = string, email = string } -> query { n = string }";
    assert_eq!(
        format_type("select", t, 40),
        "select : { n = string, m = int }\n  -> query {\n    id = int,\n    name = string,\n    email = string\n  }\n  -> query { n = string }"
    );
    // Operator names are kept; what does not parse stays on one line.
    assert!(
        format_type("_+_", "expr r int -> expr r int -> expr r int", 20)
            .starts_with("_+_ : expr r int\n")
    );
    assert_eq!(format_type("x", "(type error)", 80), "x : (type error)");
}

// ── generated programs ─────────────────────────────────────────────────────

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

/// Writes tokens with random trivia between them. A line break is always
/// indented: only an item's first token may be in column 0.
struct Gen {
    rng: Rng,
    out: String,
    comments: usize,
}

const OPS: &[&str] = &[
    "&", "&=", "&?", "&+", "&*", "&.", "&-", "?", "<?", "?>", "<?>", "$", ">>>", "<<<", "||", "&&",
    "==", "!=", "<", "<=", ">", ">=", "<>", "+", "-", "*", "/", "%", "??",
];
const NAMES: &[&str] = &["a", "users", "f", "_x1", "where", "select", "count"];

impl Gen {
    fn gap(&mut self) {
        match self.rng.below(10) {
            0 => {
                self.comments += 1;
                let c = format!(
                    " # c{} {}\n  ",
                    self.comments,
                    self.rng.pick(&["", "x = 1", "#"])
                );
                self.out.push_str(&c);
            }
            1 => self.out.push_str("\n    "),
            2 => self.out.push_str("   "),
            _ => self.out.push(' '),
        }
    }
    fn tok(&mut self, t: &str) {
        self.gap();
        self.out.push_str(t);
    }

    fn atom(&mut self, depth: usize) {
        let pick = if depth == 0 {
            self.rng.below(4)
        } else {
            self.rng.below(9)
        };
        match pick {
            0 => {
                let n = self.rng.pick(NAMES);
                self.tok(n)
            }
            1 => {
                let l = self
                    .rng
                    .pick(&["0", "42", "1.5", "2.0e3", "\"s\"", "\"a\\\"b\"", "\"\""]);
                self.tok(l)
            }
            2 => {
                let f = self.rng.pick(&[".id", ".<k", ".>k", ".name"]);
                self.tok(f)
            }
            3 => {
                let p = self.rng.pick(&["m.x", "m.x.y"]);
                self.tok(p)
            }
            4 => {
                self.tok("(");
                self.expr(depth - 1);
                self.tok(")");
            }
            5 => {
                self.tok("{");
                for i in 0..self.rng.below(4) {
                    if i > 0 {
                        self.tok(",");
                    }
                    let k = self.rng.pick(&["id", "n", "total"]);
                    self.tok(k);
                    self.tok("=");
                    self.expr(depth - 1);
                }
                self.tok("}");
            }
            6 => {
                self.tok("[");
                for i in 0..self.rng.below(4) {
                    if i > 0 {
                        self.tok(",");
                    }
                    self.expr(depth - 1);
                }
                self.tok("]");
            }
            7 => {
                self.tok("sql");
                self.tok("\"$1 + 1\"");
            }
            _ => {
                // A lambda takes everything after it, so it is bracketed.
                self.tok("(");
                self.tok("x");
                self.tok("=>");
                self.expr(depth - 1);
                self.tok(")");
            }
        }
    }

    /// Application, an operator chain, or a negation.
    fn expr(&mut self, depth: usize) {
        if self.rng.chance(15) {
            self.tok("-");
        }
        self.atom(depth);
        if depth > 0 && self.rng.chance(30) {
            for _ in 0..1 + self.rng.below(3) {
                self.atom(depth - 1);
            }
        }
        if depth > 0 && self.rng.chance(40) {
            for _ in 0..1 + self.rng.below(3) {
                let op = self.rng.pick(OPS);
                self.tok(op);
                if self.rng.chance(10) {
                    self.tok("-");
                }
                self.atom(depth - 1);
            }
        }
    }

    fn ty(&mut self, depth: usize) {
        match if depth == 0 { 0 } else { self.rng.below(4) } {
            0 => {
                let t = self.rng.pick(&["int", "r", "string"]);
                self.tok(t)
            }
            1 => {
                let head = self.rng.pick(&["query", "expr r", "maybe", "list"]);
                self.tok(head);
                self.ty_atom(depth - 1);
            }
            2 => {
                self.ty_atom(depth - 1);
                self.tok("->");
                self.ty(depth - 1);
            }
            _ => self.ty_atom(depth - 1),
        }
    }

    fn ty_atom(&mut self, depth: usize) {
        match if depth == 0 { 0 } else { self.rng.below(3) } {
            0 => {
                let t = self.rng.pick(&["int", "a", "bool"]);
                self.tok(t)
            }
            1 => {
                self.tok("(");
                self.ty(depth - 1);
                self.tok(")");
            }
            _ => {
                self.tok("{");
                let n = self.rng.below(3);
                for i in 0..n {
                    if i > 0 {
                        self.tok(",");
                    }
                    let k = self.rng.pick(&["id", "v"]);
                    self.tok(k);
                    self.tok("=");
                    self.ty(depth - 1);
                }
                if self.rng.chance(40) {
                    self.tok("|");
                    self.tok("r");
                }
                self.tok("}");
            }
        }
    }

    fn program(&mut self) {
        for i in 0..1 + self.rng.below(4) {
            if i > 0 {
                self.out
                    .push_str(self.rng.pick(&["\n", "\n\n", "\n# own line\n"]));
            }
            if self.rng.chance(15) {
                self.out.push_str("import");
                self.tok("\"lib.cagara\"");
                if self.rng.chance(50) {
                    self.tok("as");
                    self.tok("lib");
                }
                continue;
            }
            self.out.push_str(self.rng.pick(&["q", "x", "_+_"]));
            if self.rng.chance(40) {
                self.tok(":");
                self.ty(3);
            }
            self.tok("=");
            self.expr(3);
        }
        self.out.push('\n');
    }
}

fn comment_texts(src: &str) -> Vec<String> {
    let mut cs: Vec<String> = cagara_syntax::lexer::lex(src)
        .into_iter()
        .filter(|l| l.kind == cagara_syntax::lexer::Token::Comment)
        .map(|l| l.text.trim_end().to_string())
        .collect();
    cs.sort();
    cs
}

#[test]
fn generated_programs_format_losslessly_and_stably() {
    for seed in 1..=600u64 {
        let mut g = Gen {
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            out: String::new(),
            comments: 0,
        };
        g.program();
        let src = g.out;
        let p = parse(&src);
        assert!(
            p.errors.is_empty(),
            "seed {seed}: generator made {:?}\n{src}",
            p.errors
        );
        assert_eq!(
            p.syntax().text().to_string(),
            src,
            "seed {seed}: not lossless"
        );
        let out = format(&src)
            .unwrap_or_else(|_| panic!("seed {seed}: formatter changed the program\n{src}"))
            .text;
        assert!(
            parse(&out).errors.is_empty(),
            "seed {seed}:\n{src}\n=>\n{out}"
        );
        assert_eq!(
            comment_texts(&src),
            comment_texts(&out),
            "seed {seed}:\n{src}\n=>\n{out}"
        );
        let again = format(&out).expect("formatter changed its own output").text;
        assert_eq!(out, again, "seed {seed}: not idempotent\n{src}");
    }
}

#[test]
fn token_soup_never_panics_and_stays_lossless() {
    const SOUP: &[&str] = &[
        "x",
        "=",
        ":",
        "(",
        ")",
        "{",
        "}",
        "[",
        "]",
        ",",
        "|",
        "->",
        "=>",
        "&",
        "+",
        "-",
        "<>",
        ".a",
        "m.b",
        "1",
        "9999999999999999999",
        "\"s\"",
        "\"open",
        "sql",
        "import",
        "as",
        "@",
        "# c",
        "\n",
        "\n  ",
        " ",
        "\t",
    ];
    for seed in 1..=2000u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let mut src = String::new();
        for _ in 0..rng.below(40) {
            src.push_str(rng.pick(SOUP));
            if rng.chance(70) {
                src.push(' ');
            }
        }
        let p = parse(&src);
        assert_eq!(p.syntax().text().to_string(), src, "seed {seed}");
        let _ = cagara_syntax::ast::lower_source(&src);
        let out = format(&src)
            .unwrap_or_else(|_| panic!("seed {seed}: {src:?}"))
            .text;
        assert_eq!(
            comment_texts(&src),
            comment_texts(&out),
            "seed {seed}: {src:?}"
        );
        let again = format(&out)
            .unwrap_or_else(|_| panic!("seed {seed}: {out:?}"))
            .text;
        assert_eq!(out, again, "seed {seed}: {src:?}");
    }
}

/// The shape guard is the formatter's last line of defence, and its failure
/// path is what a bug report would show. It must name the difference and keep
/// the output it refused to return.
#[test]
fn the_shape_guard_reports_a_changed_program() {
    let src = "a = 1\n";
    let root = parse(src).syntax();
    // Deliberately wrong output: a different definition name.
    let err = verify_shape(&root, "b = 1\n").expect_err("the guard must fire");
    assert!(
        err.difference.contains("first difference"),
        "{}",
        err.difference
    );
    assert_eq!(err.text, "b = 1\n");
    // The correct output passes.
    assert!(verify_shape(&root, src).is_ok());
}
