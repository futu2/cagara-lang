// Test code may assert with `unwrap`, `expect`, and indexing: a panic here
// is a failing test, not a broken formatter.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

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
    // The reference's own `case` example, written the way it documents.
    (
        "reference",
        "orders : query { id = int, amount = float, status = string } = table \"public\" \"orders\"

main = orders
  & select {
    .id,
    bucket = case {
      .amount > 1000.0 => \"huge\";
      .amount > 100.0 => \"big\";
      true => \"small\"
    }
  }
",
    ),
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
        // A `case` gets its own layout: `case` and `{` spaced, arms
        // normalized, and the whole thing reformatted rather than printed as
        // written.
        (
            "q=t&select{b=case{.a>1=>\"x\";true=>\"y\"}}\n",
            "q = t & select { b = case { .a > 1 => \"x\"; true => \"y\" } }\n",
        ),
        // A `let` keeps its binding and body, with the `in` clause on one line
        // when it fits.
        (
            "q=t&select{x=let y=.a in y+1}\n",
            "q = t & select { x = let y = .a in y + 1 }\n",
        ),
        // `infixl` / `infixr` are ordinary names now, not declarations, so a
        // line beginning with one is a plain definition and formats as one.
        ("infixl=1\n", "infixl = 1\n"),
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
fn long_cases_break_one_arm_per_line() {
    // A `case` that does not fit breaks after each `;`, the way a record
    // breaks after each `,`. Each arm gets a line of its own indented under
    // the `case`, `}` closes the last one, and no arm is left hanging off the
    // `case {` line.
    check(
        "q = t & select { b = case { a_very_long_condition_name_here > 1 => \"first_result\"; another_long_condition > 2 => \"second_result\"; true => \"fallback\" } }\n",
        "q = t
  & select {
    b = case {
      a_very_long_condition_name_here > 1 => \"first_result\";
      another_long_condition > 2 => \"second_result\";
      true => \"fallback\"
    }
  }
",
    );
    // A short one stays on one line, and a lone default arm has nothing to
    // break after however long the value is.
    check(
        "q=t&select{b=case{.a>1=>\"x\";true=>\"y\"}}\n",
        "q = t & select { b = case { .a > 1 => \"x\"; true => \"y\" } }\n",
    );
    check(
        "q = t & select { b = case { true => 1 } }\n",
        "q = t & select { b = case { true => 1 } }\n",
    );
    // An arm's own operands may break without splitting the arm: `=>` is a
    // fixed separator, so `cond` and `value` stay on either side of it.
    let out = fmt(
        "q = t & select { b = case { .a_very_long_column_name > 1 => upper (concat \"a\" (concat \"b\" .name)); true => \"y\" } }\n",
    );
    assert!(out.contains("=> upper"), "{out}");
}

/// A `case` written one arm per line — the form the language reference and
/// `LEARN.md` use — is already canonical, and must not be folded back onto one
/// line or have its first arm left on the `case {` line.
#[test]
fn a_case_written_one_arm_per_line_is_canonical() {
    let want = "q = t
  & select {
    b = case {
      .amount > 1000.0 => \"huge\";
      .amount > 100.0 => \"big\";
      true => \"small\"
    }
  }
";
    // Already laid out this way: stays byte-identical.
    check(want, want);
    // Written flat, but too long for one line: the whole case breaks, arms
    // indented under `case` and `}` closing the last arm, not hanging off it.
    check(
        "q = t & select { b = case { .amount_in_cents >= 1000000 => \"huge\"; .amount_in_cents >= 100000 => \"big\"; true => \"small\" } }\n",
        "q = t
  & select {
    b = case {
      .amount_in_cents >= 1000000 => \"huge\";
      .amount_in_cents >= 100000 => \"big\";
      true => \"small\"
    }
  }
",
    );
    // Broken, an over-long arm is laid out on its own line; its `&&` chain
    // stays on that line, since an arm belongs together.
    let out = fmt(
        "v = case { .a_very_long_condition_name_here > 1 => 2; .b_very_long_column_name_x > 2 && .c_also_long > 3 => 4; true => 5 }\n",
    );
    assert_eq!(
        out,
        "v =
  case {
    .a_very_long_condition_name_here > 1 => 2;
    .b_very_long_column_name_x > 2 && .c_also_long > 3 => 4;
    true => 5
  }
"
    );
    // An arm's own `&&` chain is part of the arm: it stays on the arm's line
    // and does not open lines of its own at the arms' level.
    let out = fmt(
        "v = case { .a_very_long_condition_name_here > 1 => .some_long_name && .another_long_name && .third_long_name; true => 5 }\n",
    );
    assert!(out.contains("=> .some_long_name"), "{out}");
    // The body moved down, so the closing brace is indented with the `case`,
    // and it closes the last arm on its own line.
    assert!(out.trim_end().ends_with("\n  }"), "{out}");
}

/// The trailing `;` and comments around arms keep their place when the case
/// breaks: a comment after an arm's `;` stays that arm's comment, an own-line
/// comment keeps its line, and no `}` opens a line at the arms' own level.
#[test]
fn case_comments_and_trailing_semicolons_keep_their_place() {
    // Indentation follows the layout, not the input: a `case` over several
    // lines is laid out over them, here inside a two-stage pipeline.
    check(
        "q = t & select {\n    b = case {\n        .a > 1 => 1;  # low\n        .a > 10 => 10; # high\n        true => 0\n    }\n}\n",
        "q = t
  & select {
    b = case {
      .a > 1 => 1; # low
      .a > 10 => 10; # high
      true => 0
    }
  }
",
    );
    check(
        "q = case {\n  # why\n  .a => 1; true => 2 }\n",
        "q =\n  case {\n    # why\n    .a => 1;\n    true => 2\n  }\n",
    );
    // A trailing `;` on a flat case is kept, and never left to open a line
    // when the case breaks.
    check("q = case { .a => 1; }\n", "q = case { .a => 1; }\n");
    check(
        "q = case { a_very_long_condition_name_here > 1 => \"first\"; another_long_condition_name > 2 => \"second\"; }\n",
        "q =\n  case {\n    a_very_long_condition_name_here > 1 => \"first\";\n    another_long_condition_name > 2 => \"second\";\n  }\n",
    );
    // Blank lines inside a case do not survive, but no arm is lost: the
    // arms that fit on one line do.
    let out = fmt("q = case {\n  .a => 1;\n\n\n  true => 2\n}\n");
    assert_eq!(out, "q = case { .a => 1; true => 2 }\n");
}

/// `name = a => body` is on one line when it fits, however it was written:
/// with or without a signature, with the body on the `=>` line or on the next
/// one. Only width decides, so the spellings of one definition all format to
/// the same text.
#[test]
fn a_lambda_body_stays_on_the_arrow_line_while_it_fits() {
    // Without a signature, the body rejoins the arrow line.
    check("f = a => a + 1\n", "f = a => a + 1\n");
    check("f = a =>\n  a + 1\n", "f = a => a + 1\n");
    // With a signature the definition stays one line, as §1.1's `addOne`
    // does, whether the source moved the body or not.
    for src in [
        "f : expr r int -> expr r int = a => a + 1\n",
        "f : expr r int -> expr r int = a =>\n  a + 1\n",
    ] {
        check(src, "f : expr r int -> expr r int = a => a + 1\n");
    }
    // A record body is hugged to the arrow; it folds like any record that
    // fits and breaks on its own when it does not (see the long-lambda test).
    check("f = a => {\n  x = a\n}\n", "f = a => { x = a }\n");
}

/// A lambda that does not fit keeps its parameters on the `=` line and moves
/// the body to an indented line, the shape §1.1 shows. A body that brings its
/// own lines (`case`, a record, a pipeline) is hugged instead and breaks
/// itself, so `a =>` is never left alone on a line.
#[test]
fn a_long_lambda_moves_its_body_to_an_indented_line() {
    check(
        "f = a => b => where (.id == a && .name == b && .status == \"paid\" && .region == \"eu\" && .tier == \"gold\") q\n",
        "f = a => b =>\n  where (.id == a && .name == b && .status == \"paid\" && .region == \"eu\" && .tier == \"gold\") q\n",
    );
    // The body's own braces break, and the parameters stay with the arrow.
    check(
        "f = a => [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]\n",
        "f = a => [\n  1,\n  2,\n  3,\n  4,\n  5,\n  6,\n  7,\n  8,\n  9,\n  10,\n  11,\n  12,\n  13,\n  14,\n  15,\n  16,\n  17,\n  18,\n  19,\n  20,\n  21,\n  22,\n  23,\n  24,\n  25,\n  26,\n  27,\n  28,\n  29,\n  30,\n  31\n]\n",
    );
}

/// `(x : ty) => body`: the parameter and its annotation are one head, so the
/// arrow never separates them and a long type breaks the way a signature's
/// does rather than pushing the arrow away.
#[test]
fn an_annotated_parameter_stays_with_its_arrow() {
    check("f = (x : int) => x + 1\n", "f = (x : int) => x + 1\n");
    // Spacing inside the parentheses is normalised, and an annotation written
    // over several lines is pulled back together when it fits.
    check("f = ( x:int ) => x\n", "f = (x : int) => x\n");
    check(
        "f = (x : query {\n  id = int\n}) => x\n",
        "f = (x : query { id = int }) => x\n",
    );
    // Only the body moves when the whole lambda is too wide.
    check(
        "f = (x : query { id = int, name = string }) => where (.id == x && .name == \"a\" && .age > 18 && .region == \"eu\") q\n",
        "f = (x : query { id = int, name = string }) =>\n  where (.id == x && .name == \"a\" && .age > 18 && .region == \"eu\") q\n",
    );
}

/// A `case` used as a value (an argument, an arm of another `case`) is
/// huggable like any braced expression, and a nested one indents under its
/// arm rather than closing at the outer arm's level.
#[test]
fn a_case_nests_like_any_braced_expression() {
    check(
        "u = f case { .a => 1; true => 2 }\n",
        "u = f case { .a => 1; true => 2 }\n",
    );
    // The nested `case` is an expression like any other, so it keeps its own
    // one-line shape here: an arm does not break because the arm it is in
    // does, and it never puts its `}` on the outer arm's line.
    check(
        "v = case { .a => case { .b_very_long_name_one > 1 => \"x\"; .b_very_long_name_two > 2 => \"y\"; true => \"z\" }; true => \"fallback value here\" }\n",
        "v =
  case {
    .a => case { .b_very_long_name_one > 1 => \"x\"; .b_very_long_name_two > 2 => \"y\"; true => \"z\" };
    true => \"fallback value here\"
  }
",
    );
}

/// A long application keeps its arguments together: the body moves to an
/// indented line, and past that the arguments fill the lines rather than each
/// taking one. The old layout was all-or-nothing — one more argument turned a
/// two-line definition into one line per argument, which is both taller and
/// less readable.
#[test]
fn long_applications_fill_their_lines() {
    // Nothing is split when the body fits once it has moved down.
    let args: Vec<String> = (0..18).map(|i| format!("arg{i:02}")).collect();
    let src = format!("x = f {}\n", args.join(" "));
    check(
        &src,
        "x =
  f arg00 arg01 arg02 arg03 arg04 arg05 arg06 arg07 arg08 arg09 arg10 arg11 arg12 arg13 arg14 arg15
    arg16 arg17
",
    );
    // The one argument that does not fit breaks on its own; its neighbours stay
    // where they are.
    check(
        "x = someFunctionName arg_one arg_two arg_three arg_four arg_five arg_six arg_seven arg_eight arg_nineXYZ\n",
        "x =
  someFunctionName arg_one arg_two arg_three arg_four arg_five arg_six arg_seven arg_eight
    arg_nineXYZ
",
    );
}

/// A `case` used as a definition's body is on the `=` line while it fits, and
/// otherwise moves down whole, onto a line whose width the `case` itself gets
/// to use. As a record field it stays hugged to the field's name, because that
/// is the shape `select { b = case { ...` is written in.
#[test]
fn a_long_case_body_moves_to_an_indented_line() {
    check(
        "x = case { .a_very_long_condition_name > 1 => \"first_result\"; .b_another_long_condition > 2 => \"second\"; true => \"fallback\" }\n",
        "x =
  case {
    .a_very_long_condition_name > 1 => \"first_result\";
    .b_another_long_condition > 2 => \"second\";
    true => \"fallback\"
  }
",
    );
    // Shorter than the width: the body stays where it is, on one line.
    check(
        "x = case { .a > 1 => \"x\"; true => \"y\" }\n",
        "x = case { .a > 1 => \"x\"; true => \"y\" }\n",
    );
    // A record field keeps `b = case {` however long the case is.
    let out = fmt(
        "q = t & select { b = case { .amount_in_cents >= 1000000 => \"huge\"; .amount_in_cents >= 100000 => \"big\"; true => \"small\" } }\n",
    );
    assert!(out.contains("b = case {"), "{out}");
    assert_eq!(fmt(&out), out, "not idempotent:\n{out}");
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
    // The arguments fill the indented line rather than taking one line each:
    // `someFunction` and the first two fit there, and only the third breaks.
    check(
        &src,
        &format!(
            "x =\n  someFunction {} {}\n    {}\n",
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

/// A file full of unparseable items is formatted in time proportional to its
/// size, not to its size times its item count.
///
/// Every item the formatter cannot lay out is printed verbatim, and `verbatim`
/// sliced the **root's** text once per item. `SyntaxText::slice` walks the green
/// children from the node it hangs off, so each slice cost a walk of the whole
/// file: 20 000 broken items took 46 seconds where parsing them took
/// milliseconds, and the language server's Format Document inherits it. Slicing
/// the item's own text instead made the same file 1.2 seconds.
///
/// The bound is deliberately loose — it is here to catch a return to quadratic
/// behaviour, not to measure the machine.
#[test]
fn a_file_of_unparseable_items_is_formatted_in_linear_time() {
    let src: String = (0..20_000).map(|i| format!("d{i} =\n")).collect();
    let start = std::time::Instant::now();
    let out = format(&src).expect("verbatim output still round-trips");
    let elapsed = start.elapsed();
    assert_eq!(out.text.lines().count(), 20_000);
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "formatting 20 000 broken items took {elapsed:?}; it was 46s when \
         `verbatim` sliced the root once per item"
    );
}
