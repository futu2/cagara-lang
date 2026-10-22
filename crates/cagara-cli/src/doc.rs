//! `cagara doc`: the reference a file carries in its own comments.
//!
//! `##` documents the definition under it, `#!` documents the file (the
//! markers are described in [`cagara_syntax::docs`]). This subcommand is what
//! makes that text useful away from the source: markdown for a person, JSON
//! with `--json` for a tool, and `--check` for the one question a writer asks
//! before publishing — which top-level definitions have no documentation yet.
//!
//! `--check` **warns** and exits 0. A missing doc comment is not a compile
//! error, and the definitions a project has not got round to describing are
//! exactly the ones a build must not block on; the point of the report is to
//! make the gap visible, not to gate the branch.
//!
//! The signature printed for each definition is the inferred one, the same
//! text `cagara --types` prints. Writing docs from the signature rather than
//! from the source means an undocumented definition still shows its type, and
//! a documented one cannot disagree with the checker.

use cagara_hir::{check, Workspace};
use cagara_syntax::docs::{item_doc, module_doc, Docs, FieldDoc};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

const DOC_USAGE: &str = "usage: cagara doc <file.cagara...|-> [--json]
       cagara doc --prelude [--json]
       cagara doc <file.cagara...|-> --check    report undocumented definitions (warns, does not fail)";

/// One module's documentation, in the shape the renderers read.
///
/// Owned rather than borrowed from the workspace so a run over several files
/// can hold every module's reference until the last one is rendered.
struct Documented {
    /// The path as the user named it, which is what the output should show.
    path: String,
    doc: Option<String>,
    defs: Vec<DocumentedDef>,
}

struct DocumentedDef {
    name: String,
    /// The inferred type, or `None` when there is none to report (a type
    /// error, or a definition the checker did not reach).
    ty: Option<String>,
    doc: Option<String>,
    /// The record types in the definition's declared signature, each with its
    /// fields: a table's columns are named here, so this is where a column's
    /// documentation is found.
    fields: Vec<Vec<FieldDoc>>,
    /// 1-based line of the definition, for `--check`'s report.
    line: usize,
}

/// What parsing `cagara doc`'s arguments produced.
enum Parsed {
    Options(Options),
    Help,
    Usage(String),
}

struct Options {
    /// One entry per file to document: the path to load, and the name to show.
    paths: Vec<(PathBuf, String)>,
    prelude: bool,
    check: bool,
    json: bool,
}

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let opts = match parse(args) {
        Parsed::Options(opts) => opts,
        Parsed::Help => return crate::out(DOC_USAGE).map(|()| ExitCode::SUCCESS),
        Parsed::Usage(message) => {
            eprintln!("error: {message}\n{DOC_USAGE}");
            return Ok(ExitCode::from(2));
        }
    };

    // A file that cannot be read or parsed is reported and skipped: the
    // definitions a broken parse did see are not a reference anyone should
    // publish, and "undocumented" is not a claim to make about them.
    let mut failed = false;
    let mut documented: Vec<Documented> = Vec::new();
    if opts.prelude {
        let ws = Workspace::prelude();
        document(&ws, "<prelude>", &opts, &mut documented, &mut failed)?;
    }
    for (path, shown) in &opts.paths {
        match crate::open_source(path) {
            Ok(ws) => document(&ws, shown, &opts, &mut documented, &mut failed)?,
            Err(e) => {
                eprintln!("{shown}: {e}");
                failed = true;
            }
        }
    }

    if opts.check {
        // Warnings only: `failed` is whatever loading said, so a file that does
        // not parse fails this and an undocumented one does not.
        return Ok(crate::done(failed));
    }

    let text = if opts.json {
        json(&documented)
    } else {
        markdown(&documented)
    };
    if !text.is_empty() {
        crate::out(&text)?;
    }
    Ok(crate::done(failed))
}

/// Document one workspace's root module: collect it, or with `--check` report
/// the definitions that carry no documentation.
fn document(
    ws: &Workspace,
    shown: &str,
    opts: &Options,
    out: &mut Vec<Documented>,
    failed: &mut bool,
) -> io::Result<()> {
    if !ws.diags.is_empty() {
        for d in &ws.diags {
            eprintln!("{d}");
        }
        *failed = true;
        return Ok(());
    }
    let tc = check(ws);
    // Root errors are reported and the documentation is still printed: a
    // reference is most useful for a file that is still being written. Errors
    // in imported modules belong to those modules, and the prelude is checked
    // on every run, so repeating them here would bury the file's own.
    for e in tc.errors().iter().filter(|e| e.module == ws.root) {
        eprintln!("{}", e.diag);
        *failed = true;
    }
    let module = &ws.modules[ws.root];
    let lines = Docs::new(&module.text);
    let defs: Vec<DocumentedDef> = module
        .module
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| DocumentedDef {
            name: d.name.clone(),
            ty: tc.type_of(ws.root, i).map(str::to_string),
            // Read from the text at the definition's own span: the span is the
            // whole connection between the two (see `cagara_syntax::docs`).
            doc: item_doc(&module.text, d.span.start as usize),
            // A field has no item of its own: its documentation lives in the
            // signature that names it.
            fields: d
                .ty
                .as_ref()
                .map_or_else(Vec::new, |ty| lines.record_fields(ty)),
            line: lines.line(d.span.start as usize),
        })
        .collect();
    if opts.check {
        warn_missing(shown, &defs);
        return Ok(());
    }
    out.push(Documented {
        path: shown.to_string(),
        doc: module_doc(&module.text, &module.module),
        defs,
    });
    Ok(())
}

/// `--check`: say which names carry no `##` documentation.
///
/// One warning per name rather than per definition: an overload set is one
/// thing to document, and four warnings for `toDecimal` would read as four
/// separate omissions.
fn warn_missing(shown: &str, defs: &[DocumentedDef]) {
    for group in groups(defs) {
        let Some(first) = group.first() else {
            continue;
        };
        if group.iter().any(|d| d.doc.is_some()) {
            continue;
        }
        // The first definition of the name, which is where the doc block would
        // go.
        eprintln!(
            "{shown}:{}: warning: `{}` has no doc comment; put `## ...` on the line above it \
             (see `cagara doc --help`)",
            first.line, first.name
        );
    }
    for d in defs {
        warn_partial_fields(shown, d);
    }
}

/// A field list that documents some of its columns is almost certainly an
/// oversight, so this reports the rest of them.
///
/// Nothing is said about a record with no field documentation at all: a table
/// whose columns are self-evident is not a gap, and warning about every field of
/// every signature would bury the file's own missing definitions.
fn warn_partial_fields(shown: &str, def: &DocumentedDef) {
    for record in &def.fields {
        if record.iter().all(|f| f.doc.is_none()) {
            continue;
        }
        for f in record.iter().filter(|f| f.doc.is_none()) {
            eprintln!(
                "{shown}:{}: warning: field `{}` has no doc comment, and other fields of this \
                 record type do; put `## ...` on the line above the field, or drop the field \
                 notes from this record",
                f.line, f.name
            );
        }
    }
}

/// The reference as markdown: one section per name, in source order, with the
/// signature(s) in a fence under the heading.
///
/// Adjacent definitions of one name are one section. That is what an overload
/// set is — `sum` for `int` and for `float` are one thing to a reader — and
/// documenting the set once, above its first definition, is how the prelude is
/// written. A definition without documentation still gets its section: the list
/// of what a file *provides* is worth having on its own, and a gap in the prose
/// is easier to see against a complete list than against a short one.
fn markdown(modules: &[Documented]) -> String {
    let mut out = String::new();
    for (i, m) in modules.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("# {}\n", m.path));
        if let Some(doc) = &m.doc {
            out.push_str(&format!("\n{doc}\n"));
        }
        for group in groups(&m.defs) {
            let Some(first) = group.first() else {
                continue;
            };
            out.push_str(&format!("\n## `{}`\n\n```text\n", first.name));
            for d in group {
                match &d.ty {
                    Some(ty) => out.push_str(&format!("{} : {ty}\n", d.name)),
                    None => out.push_str(&format!("{}\n", d.name)),
                }
            }
            out.push_str("```\n");
            for doc in group.iter().filter_map(|d| d.doc.as_deref()) {
                out.push_str(&format!("\n{doc}\n"));
            }
            let fields = field_notes(group);
            if !fields.is_empty() {
                out.push_str("\nFields:\n\n");
                for (name, doc) in fields {
                    // A field's note may be several lines; under a bullet, the
                    // continuation is indented to stay inside the item.
                    out.push_str(&format!("- `{name}` — {}\n", doc.replace('\n', "\n  ")));
                }
            }
        }
    }
    out
}

/// The documented fields of an overload set's declared signatures, in source
/// order, as `(name, doc)`.
///
/// Only the documented ones: the signature above already lists every field, so
/// repeating the undocumented names in prose would add lines without adding
/// information.
fn field_notes(group: &[DocumentedDef]) -> Vec<(&str, &str)> {
    group
        .iter()
        .flat_map(|d| &d.fields)
        .flatten()
        .filter_map(|f| Some((f.name.as_str(), f.doc.as_deref()?)))
        .collect()
}

/// Split definitions into runs of one name: an overload set, or a single
/// definition, in source order.
fn groups(defs: &[DocumentedDef]) -> Vec<&[DocumentedDef]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=defs.len() {
        let same = defs
            .get(i)
            .zip(defs.get(start))
            .is_some_and(|(next, first)| next.name == first.name);
        if !same {
            if let Some(run) = defs.get(start..i) {
                out.push(run);
            }
            start = i;
        }
    }
    out
}

/// The reference as JSON, for a tool that would rather not parse markdown: a
/// documentation site, an editor extension, a check that every name a release
/// added is described.
fn json(modules: &[Documented]) -> String {
    let mut out = String::from("{\n  \"modules\": [");
    for (i, m) in modules.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\n    {{\n      \"path\": {},\n      \"doc\": {},\n      \"definitions\": [",
            string(&m.path),
            option(m.doc.as_deref())
        ));
        for (j, d) in m.defs.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n        {{\"name\": {}, \"type\": {}, \"line\": {}, \"doc\": {}, \"fields\": [",
                string(&d.name),
                option(d.ty.as_deref()),
                d.line,
                option(d.doc.as_deref())
            ));
            let fields: Vec<String> = d
                .fields
                .iter()
                .flatten()
                .map(|f| {
                    format!(
                        "{{\"name\": {}, \"line\": {}, \"doc\": {}}}",
                        string(&f.name),
                        f.line,
                        option(f.doc.as_deref())
                    )
                })
                .collect();
            out.push_str(&fields.join(", "));
            out.push_str("]}");
        }
        out.push_str("\n      ]\n    }");
    }
    out.push_str("\n  ]\n}");
    out
}

/// A JSON string literal.
fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn option(v: Option<&str>) -> String {
    v.map_or_else(|| "null".to_string(), string)
}

fn parse(args: impl Iterator<Item = OsString>) -> Parsed {
    let mut paths = Vec::new();
    let mut prelude = false;
    let mut check = false;
    let mut json = false;
    for a in args {
        match a.to_str() {
            Some("--prelude") => prelude = true,
            Some("--check") => check = true,
            Some("--json") => json = true,
            Some("-h") | Some("--help") => return Parsed::Help,
            // `-` is stdin, spelled as a path the rest of the shell
            // understands: `git show x:q.cagara | cagara doc -`.
            Some("-") => paths.push((PathBuf::from(crate::STDIN_PATH), "-".to_string())),
            _ if !a.to_string_lossy().starts_with('-') => {
                paths.push((PathBuf::from(&a), a.to_string_lossy().into_owned()));
            }
            _ => {
                return Parsed::Usage(format!("unexpected argument `{}`", a.to_string_lossy()));
            }
        }
    }
    match (prelude, paths.is_empty()) {
        (true, false) => {
            return Parsed::Usage("`--prelude` documents the built-in prelude, not a file".into())
        }
        (false, true) => return Parsed::Usage("missing input file".into()),
        _ => {}
    }
    if check && json {
        return Parsed::Usage("`--check` reports warnings to stderr; drop `--json`".into());
    }
    Parsed::Options(Options {
        paths,
        prelude,
        check,
        json,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn def(name: &str, doc: Option<&str>) -> DocumentedDef {
        DocumentedDef {
            name: name.into(),
            ty: Some("query r".into()),
            doc: doc.map(String::from),
            fields: Vec::new(),
            line: 1,
        }
    }

    #[test]
    fn markdown_carries_the_file_doc_signatures_and_bodies() {
        let m = Documented {
            path: "q.cagara".into(),
            doc: Some("A query file.".into()),
            defs: vec![def("main", Some("The answer.")), def("helper", None)],
        };
        assert_eq!(
            markdown(&[m]),
            "# q.cagara\n\
             \n\
             A query file.\n\
             \n\
             ## `main`\n\
             \n\
             ```text\n\
             main : query r\n\
             ```\n\
             \n\
             The answer.\n\
             \n\
             ## `helper`\n\
             \n\
             ```text\n\
             helper : query r\n\
             ```\n"
        );
    }

    /// An overload set is one section: the reader wants `sum` once, with both of
    /// its signatures, not two half-sections that each explain half of it.
    #[test]
    fn adjacent_definitions_of_one_name_are_one_section() {
        let defs = vec![
            def("sum", Some("Adds a column up.")),
            def("sum", None),
            def("count", Some("Counts rows.")),
        ];
        assert_eq!(
            groups(&defs).iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![2, 1]
        );
        let text = markdown(&[Documented {
            path: "q.cagara".into(),
            doc: None,
            defs,
        }]);
        assert_eq!(text.matches("## `sum`").count(), 1, "{text}");
        assert!(text.contains("sum : query r\nsum : query r\n"), "{text}");
        // One doc for the set is enough, and it is printed once.
        assert_eq!(text.matches("Adds a column up.").count(), 1, "{text}");
    }

    /// A definition whose type the checker could not produce still appears:
    /// the reference should not silently lose names when one of them is broken.
    #[test]
    fn a_definition_without_a_type_still_has_a_section() {
        let m = Documented {
            path: "q.cagara".into(),
            doc: None,
            defs: vec![DocumentedDef {
                name: "broken".into(),
                ty: None,
                doc: None,
                fields: Vec::new(),
                line: 7,
            }],
        };
        let text = markdown(&[m]);
        assert!(text.contains("## `broken`"), "{text}");
        assert!(text.contains("```text\nbroken\n```"), "{text}");
    }

    #[test]
    fn json_is_shaped_for_a_tool() {
        let m = Documented {
            path: "q.cagara".into(),
            doc: None,
            defs: vec![def("main", Some("Line one.\nLine \"two\"."))],
        };
        let text = json(&[m]);
        assert!(text.contains(r#""path": "q.cagara""#), "{text}");
        assert!(text.contains(r#""doc": null"#), "{text}");
        assert!(
            text.contains(r#""doc": "Line one.\nLine \"two\".""#),
            "{text}"
        );
        assert!(text.trim_end().ends_with('}'), "{text}");
    }
}
