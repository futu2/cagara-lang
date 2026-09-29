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
