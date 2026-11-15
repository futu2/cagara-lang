//! `cagara <file.cagara|-> [--dialect NAME] [--only DEF|alias.DEF] [--arg VALUE]…
//! [--pretty] [--optimize] [--types] [--list]`
//! `cagara fmt [--check] <files...|->`
//! `cagara doc <files...|-> [--json] | --prelude | --check`
//! `cagara lsp`
//!
//! Prints the SQL for the root file's `main` definition, or with `--only`
//! another definition; `--types` prints inferred types instead. A file without
//! `main` is a library and prints nothing. `cagara fmt` formats files in place.
//! `cagara doc` prints the reference a file carries in its `##` and `#!`
//! comments. `cagara lsp` runs the language server over stdio.

mod check;
mod doc;
mod fmt;

use cagara_hir::check::SchemeView;
use cagara_hir::{compile_checked, TypeCheck, Workspace};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The version this binary was built from, which is the workspace version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The name `--version` reports for the binary.
const NAME: &str = "cagara";

/// What this binary is, beyond its version: the commit it was built from, the
/// profile, the target and the time.
///
/// `--version` stays a bare version line, because that is the one thing scripts
/// parse and the reference documents. This is for the language server's first
/// log line instead, which is where a stale compiler has to be recognisable —
/// a binary that has been staged or copied cannot be told from a current one by
/// its path or its timestamp.
const BUILD: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CAGARA_BUILD_COMMIT"),
    ", ",
    env!("CAGARA_BUILD_PROFILE"),
    ", ",
    env!("CAGARA_BUILD_TARGET"),
    ", built ",
    env!("CAGARA_BUILD_DATE"),
    ")"
);

const USAGE: &str =
    "usage: cagara <file.cagara|-> [--dialect NAME] [--only DEF|alias.DEF] [--arg VALUE]…
                                   [--param NAME[=VALUE]|NAME:TYPE=VALUE]… [--pretty] [--optimize]
                                   [--types] [--list]
       cagara fmt [--check] <files...>   format files in place (`-` for stdin to stdout)
       cagara doc <files...|-> [--json]  the reference a file carries in `##` comments
       cagara doc --prelude              the built-in prelude, documented
       cagara doc <files...> --check     list definitions with no doc comment (warns only)
       cagara check [--json] <path>...    report every file under `path` that does not check
       cagara lsp    run the language server over stdio
       cagara --version";

/// The path a source read from stdin is reported as, so a diagnostic still
/// names a file. A real file of that name is not consulted: `open_source`
/// takes the text directly.
pub(crate) const STDIN_PATH: &str = "<stdin>";

/// The definition a file is compiled for, unless `--only` names another.
///
/// `main` is the entry point by convention, and the only definition whose SQL
/// is printed by default. A file that does not define one prints nothing.
const ENTRY_POINT: &str = "main";

/// The definition `--arg` builds: the selected definition applied to literals.
const ARG_ENTRY: &str = "__cagara_args";

fn main() -> ExitCode {
    match main_inner() {
        Ok(code) => code,
        // A closed reader (`cagara file.cagara | head`) ends the output, not
        // the exit status.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cagara: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main_inner() -> std::io::Result<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `--version` is answered before the subcommand dispatch, so it works
    // whatever else was passed: `cagara fmt --version` is still a version
    // question. It is the only information flag, so `-V` means it too.
    if args.iter().any(|a| is_version_flag(a)) {
        return out(&format!("{NAME} {VERSION}")).map(|()| ExitCode::SUCCESS);
    }
    let mut rest = args.into_iter();
    let first = rest.next();
    if first.as_deref() == Some(OsStr::new("fmt")) {
        return fmt::run(rest);
    }
    if first.as_deref() == Some(OsStr::new("doc")) {
        return doc::run(rest);
    }
    if first.as_deref() == Some(OsStr::new("check")) {
        return check::run(rest);
    }
    if first.as_deref() == Some(OsStr::new("lsp")) {
        let rest: Vec<OsString> = rest.collect();
        if rest
            .iter()
            .any(|a| a == OsStr::new("-h") || a == OsStr::new("--help"))
        {
            return out(USAGE).map(|()| ExitCode::SUCCESS);
        }
        // Editors may pass `--stdio`; stdio is the only transport.
        if let Some(a) = rest.iter().find(|a| *a != OsStr::new("--stdio")) {
            return Ok(usage(&format!(
                "unexpected argument `{}` for `cagara lsp`",
                display(a)
            )));
        }
        return match cagara_lsp::run(BUILD) {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => {
                eprintln!("cagara lsp: {e}");
                Ok(ExitCode::FAILURE)
            }
        };
    }
    compile(first.into_iter().chain(rest))
}

/// Whether `a` asks for the version (`--version` / `-V`).
fn is_version_flag(a: &OsStr) -> bool {
    a == OsStr::new("--version") || a == OsStr::new("-V")
}

/// `cagara FILE` options, parsed and validated.
struct Options {
    file: PathBuf,
    dialect: cagara_sql::Dialect,
    only: Option<String>,
    pretty: bool,
    types: bool,
    optimize: bool,
    /// Print every root query, not only the entry point.
    list: bool,
    /// Literal arguments for the selected definition, applied in order.
    values: Vec<String>,
    /// `--param name=value` (or `name:type=value`): definitions the file leaves
    /// to its caller. Written into the file as ordinary definitions, so a
    /// parameter is a name like any other.
    params: Vec<String>,
    /// The name to label the output with when `--arg` built a call: the
    /// definition the user asked for, not the wrapper.
    label: Option<String>,
}

/// What parsing `cagara FILE`'s arguments produced.
///
/// Parsing does not print: `compile` owns the output so an I/O error (a closed
/// reader) is handled the same way as everywhere else.
enum Parsed {
    Options(Options),
    /// `--help`: print the usage text and succeed.
    Help,
    /// A usage error: print the message and the usage text, exit 2.
    Usage(String),
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Parsed {
    let mut file: Option<PathBuf> = None;
    let mut dialect_name = String::from("ansi");
    let mut only: Option<String> = None;
    let mut pretty = false;
    let mut types = false;
    let mut optimize = false;
    let mut list = false;
    let mut values: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--dialect") => match args.next() {
                Some(d) => match d.into_string() {
                    Ok(d) => dialect_name = d,
                    Err(d) => {
                        return Parsed::Usage(format!(
                            "`--dialect` is not valid UTF-8: `{}`",
                            display(&d)
                        ))
                    }
                },
                None => return Parsed::Usage("--dialect needs a value".into()),
            },
            Some("--only") => match args.next() {
                Some(d) => match d.into_string() {
                    Ok(d) => only = Some(d),
                    Err(d) => {
                        return Parsed::Usage(format!(
                            "`--only` is not valid UTF-8: `{}`",
                            display(&d)
                        ))
                    }
                },
                None => return Parsed::Usage("--only needs a definition name".into()),
            },
            Some("--arg") => match args.next() {
                Some(d) => match d.into_string() {
                    Ok(d) => values.push(d),
                    Err(d) => {
                        return Parsed::Usage(format!(
                            "`--arg` is not valid UTF-8: `{}`",
                            display(&d)
                        ))
                    }
                },
                None => return Parsed::Usage("--arg needs a value".into()),
            },
            Some("--param") => match args.next() {
                Some(d) => match d.into_string() {
                    Ok(d) => match parse_param(&d) {
                        Ok(_) => params.push(d),
                        Err(e) => return Parsed::Usage(e),
                    },
                    Err(d) => {
                        return Parsed::Usage(format!(
                            "`--param` is not valid UTF-8: `{}`",
                            display(&d)
                        ))
                    }
                },
                None => return Parsed::Usage("--param needs name=value".into()),
            },
            Some("--list") => list = true,
            Some("--pretty") => pretty = true,
            Some("--types") => types = true,
            Some("--optimize") => optimize = true,
            Some("-h") | Some("--help") => return Parsed::Help,
            _ if file.is_none() && !a.to_string_lossy().starts_with("--") => {
                file = Some(PathBuf::from(a))
            }
            _ => return Parsed::Usage(format!("unexpected argument `{}`", display(&a))),
        }
    }
    let Some(file) = file else {
        return Parsed::Usage("missing input file".into());
    };
    // `-` is stdin, spelled as a path the rest of the shell understands: a
    // pipeline (`git show x:q.cagara | cagara -`) and an editor filter both
    // need it, and so does `fmt`, which already documents it.
    let file = if file == Path::new("-") {
        PathBuf::from(STDIN_PATH)
    } else {
        file
    };
    let Some(dialect) = cagara_sql::dialect(&dialect_name) else {
        return Parsed::Usage(format!("unknown dialect `{dialect_name}`"));
    };
    Parsed::Options(Options {
        file,
        dialect,
        only,
        pretty,
        types,
        optimize,
        list,
        values,
        params,
        label: None,
    })
}

fn compile(args: impl Iterator<Item = OsString>) -> std::io::Result<ExitCode> {
    let opts = match parse_args(args) {
        Parsed::Options(opts) => opts,
        Parsed::Help => {
            out(USAGE)?;
            return Ok(ExitCode::SUCCESS);
        }
        Parsed::Usage(message) => return Ok(usage(&message)),
    };

    let mut opts = opts;
    // Two requests need a definition that is not in the file: arguments for a
    // definition (`--arg`), and a definition in an imported module
    // (`--only alias.name`). Both are one more line at the end of the file —
    // `__cagara_args = <qualified name> <literal> …` — which needs no compiler
    // support: it is compiled like any other definition, so a bad argument or
    // an unknown name is reported by the ordinary checker.
    let qualified = opts.only.clone().unwrap_or_else(|| ENTRY_POINT.to_string());
    let mut params = String::new();
    for flag in &opts.params {
        match param_definition(flag) {
            Ok(def) => params.push_str(&def),
            // `parse_param` already validated the shape when the flag was read.
            Err(e) => return Ok(usage(&e)),
        }
    }
    if !opts.values.is_empty() || !opts.params.is_empty() || qualified.contains('.') {
        let body = call_with_literals(&qualified, &opts.values);
        opts.only = Some(ARG_ENTRY.to_string());
        opts.label = Some(qualified);
        let ws = match open_with_wrapper(&opts.file, &body, &params) {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("{STDIN_PATH}: {e}");
                return Ok(ExitCode::FAILURE);
            }
        };
        return run(&ws, &opts);
    }
    let ws = match open_source(&opts.file) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("{STDIN_PATH}: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    run(&ws, &opts)
}

/// Check and report a loaded workspace. Shared by the plain path and the
/// `--arg` path, so the two cannot drift.
fn run(ws: &Workspace, opts: &Options) -> std::io::Result<ExitCode> {
    if !ws.diags.is_empty() {
        for d in &ws.diags {
            eprintln!("{d}");
        }
        return Ok(ExitCode::FAILURE);
    }

    // Errors in imported modules are always reported; root errors are
    // reported per definition below (so `--only` applies to them).
    let tc = cagara_hir::check(ws);
    let mut failed = false;
    // Warnings first, so a file that has both reads top-down: they do not fail
    // the run, and their position in the output is the only thing that says so.
    for w in tc.warnings() {
        eprintln!("{}", w.diag);
    }
    for e in tc.errors().iter().filter(|e| e.module != ws.root) {
        eprintln!("{}", e.diag);
        failed = true;
    }
    // A parameter that does not check is the *flag's* problem, not the file's:
    // the definition it came from is not in the file, so its line number would
    // point past the end of what the user wrote.
    for flag in &opts.params {
        let Ok((name, _, _)) = parse_param(flag) else {
            continue;
        };
        let Some(i) = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
        else {
            continue;
        };
        if let Some(e) = tc.error_for(ws.root, i) {
            eprintln!("error: `--param {flag}`: {}", e.message);
            failed = true;
        }
    }

    if opts.types {
        report_types(ws, &tc, opts, failed)
    } else {
        report_sql(ws, &tc, opts, failed)
    }
}

/// Open `path` as a workspace, reading stdin when it is [`STDIN_PATH`].
///
/// `Workspace::open` reads from disk, so stdin is read here and handed to
/// `open_with` — which keeps imports resolving relative to the current
/// directory, exactly as `from_source` does.
pub(crate) fn open_source(path: &Path) -> std::io::Result<Workspace> {
    if path != Path::new(STDIN_PATH) {
        return Ok(Workspace::open(path));
    }
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(Workspace::open_with(path, text))
}

/// Open `path` with one more definition that applies `selected` to `values`.
///
/// Each value is written as the literal it looks like — a number, a `true`/
/// `false`, otherwise a quoted string — because a definition's parameters are
/// ordinary values and the checker decides what fits (`date` accepts a string
/// literal, `string` accepts a string).
fn open_with_wrapper(path: &Path, body: &str, params: &str) -> std::io::Result<Workspace> {
    let mut text = String::new();
    if path == Path::new(STDIN_PATH) {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text = std::fs::read_to_string(path)?;
    }
    // A `--param` naming a definition the *file* declares overrides that
    // definition's value, keeping its type.
    //
    // A parameter is a definition the file leaves to its caller, and the natural
    // way to write one is in the file with a default — `limit_n : int = 5` — so
    // `cagara q.cagara` works and `--param limit_n=2` changes it. Prepending the
    // flag's definition instead collided with the file's own: `limit_n` would be
    // defined twice, which is an overloading error, so a file could either declare
    // its parameters or be overridden, never both.
    //
    // The replacement is textual and uses the definition's own body span, so the
    // signature — and the doc comment above it — stay exactly as written. A wrong
    // type is still the file's error, because the type is still the file's.
    //
    // Replacements are collected first and applied *from the end*, because a value
    // of a different length moves every offset after it: applying them front to back
    // made `--param limit_n=2 --param off_n=1` emit `LIMIT 1 OFFSET 1`, the first
    // value written into the second's span.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut appended = String::new();
    for def in param_definitions(params) {
        match declared_span(&text, &def.name) {
            Some((start, end)) => edits.push((start, end, def.value)),
            None => {
                // Not declared: the flag supplies the whole definition, type and all.
                appended.push_str(&match def.ty {
                    Some(ty) => format!("{} : {ty} = {}\n", def.name, def.value),
                    None => format!("{} = {}\n", def.name, def.value),
                });
            }
        }
    }
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, value) in edits {
        text.replace_range(start..end, &value);
    }
    // Parameters first: the wrapper calls a definition, and that definition may
    // read one. Cagara resolves forward references, so the order is for reading.
    text.push_str(&appended);
    text.push_str(&format!("\n{ARG_ENTRY} = {body}\n"));
    Ok(Workspace::open_with(path, text))
}

/// One `--param` flag, as the pieces the wrapper needs.
struct ParamDef {
    name: String,
    value: String,
    /// The type the flag spelled, if it spelled one. The file's own declaration is
    /// what supplies it otherwise, which is the case an override is about.
    ty: Option<String>,
}

/// The value a `--param` flag supplies, and the name it supplies it for.
fn param_definitions(params: &str) -> Vec<ParamDef> {
    params
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                return None;
            }
            let (lhs, value) = line.split_once(" = ")?;
            let (name, ty) = match lhs.split_once(" : ") {
                Some((n, t)) => (n.to_string(), Some(t.to_string())),
                None => (lhs.to_string(), None),
            };
            Some(ParamDef {
                name,
                value: value.to_string(),
                ty,
            })
        })
        .collect()
}

/// The byte range of a definition's *body* in `text`, when the file declares it.
///
/// Read from the parsed file rather than by searching the text: the body's span is
/// the one the parser recorded, so a name mentioned inside a string or a comment
/// cannot be mistaken for the definition.
fn declared_span(text: &str, name: &str) -> Option<(usize, usize)> {
    // The span of the definition whose name is `name`, in the root file only: a
    // parameter declared in an imported module is that module's business.
    let (file, _errors) = cagara_syntax::ast::lower_source(text);
    file.defs
        .iter()
        .find(|d| d.name == name)
        .map(|d| (d.body.span.start as usize, d.body.span.end as usize))
        .filter(|(start, end)| *start < text.len() && *end <= text.len() && start < end)
}

/// What a `--param` flag writes: `name = value`, or `name : type = value`.
///
/// A parameter is a **definition the file leaves to its caller**. That keeps
/// the language as it is — no new syntax, no new kind — while making the value
/// part of the compilation: it is checked against every use, and a wrong type
/// is a type error rather than a runtime surprise. `--param d:date=2024-01-01`
/// carries a signature when the literal alone would be too vague (a string
/// literal widens to `date`, but nothing widens to `decimal(18,2)`).
fn param_definition(flag: &str) -> Result<String, String> {
    let (name, ty, value) = parse_param(flag)?;
    Ok(match ty {
        Some(ty) => format!("{name} : {ty} = {value}\n"),
        None => format!("{name} = {value}\n"),
    })
}

/// `name=value` or `name:type=value`, as the flag spells it.
///
/// The value is written the way `--arg` writes one: as the literal it looks
/// like. Validated here so a typo is a usage error at the flag rather than a
/// syntax error in a file the user did not write.
fn parse_param(flag: &str) -> Result<(String, Option<String>, String), String> {
    let Some((lhs, value)) = flag.split_once('=') else {
        return Err(format!(
            "`--param {flag}` needs a value: write `--param name=value`"
        ));
    };
    let (name, ty) = match lhs.split_once(':') {
        Some((n, t)) => (n.to_string(), Some(t.to_string())),
        None => (lhs.to_string(), None),
    };
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if !ident(&name) {
        return Err(format!(
            "`--param` needs a definition name, and `{name}` is not one"
        ));
    }
    if let Some(t) = &ty {
        if t.is_empty() {
            return Err("`--param name:type=value` needs a type".into());
        }
    }
    Ok((name, ty, literal(value)))
}

/// A value written as the literal it looks like: a number, `true`/`false`, or a
/// quoted string (which widens to `date`/`timestamp` where the signature says
/// so).
fn literal(v: &str) -> String {
    let numeric = v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok();
    if numeric || v == "true" || v == "false" {
        return v.to_string();
    }
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The call `--arg` writes: `definition value value …`, each value as the
/// literal it looks like.
fn call_with_literals(selected: &str, values: &[String]) -> String {
    let mut call = String::from(selected);
    for v in values {
        call.push(' ');
        let numeric = v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok();
        if numeric || v == "true" || v == "false" {
            call.push_str(v);
        } else {
            call.push('"');
            for c in v.chars() {
                match c {
                    '\\' | '"' => {
                        call.push('\\');
                        call.push(c);
                    }
                    _ => call.push(c),
                }
            }
            call.push('"');
        }
    }
    call
}

/// The printed type of `main` when it exists, is well typed, and is not a
/// query. `None` means there is nothing to report: no `main`, a query, or a
/// definition whose own diagnostic already explains the problem.
fn main_that_is_not_a_query(ws: &Workspace, tc: &TypeCheck) -> Option<String> {
    let (def, _) = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .enumerate()
        .find(|(_, d)| d.name == ENTRY_POINT)?;
    if tc.error_for(ws.root, def).is_some() {
        return None;
    }
    match tc.scheme_view(ws.root, def)? {
        SchemeView::Query { .. } => None,
        _ => Some(tc.type_of(ws.root, def).unwrap_or("?").to_string()),
    }
}

/// `--types`: the inferred type of every selected root definition.
fn report_types(
    ws: &Workspace,
    tc: &cagara_hir::TypeCheck,
    opts: &Options,
    mut failed: bool,
) -> std::io::Result<ExitCode> {
    let mut selected = false;
    for (i, d) in ws.modules[ws.root].module.defs.iter().enumerate() {
        if opts.only.as_deref().is_some_and(|o| o != d.name) {
            continue;
        }
        selected = true;
        match (tc.error_for(ws.root, i), tc.type_of(ws.root, i)) {
            (Some(e), _) => {
                eprintln!("{e}");
                failed = true;
            }
            (None, Some(t)) => {
                let name = opts.label.clone().unwrap_or_else(|| d.name.clone());
                out(&format!("{name} : {t}"))?
            }
            (None, None) => {
                let name = opts.label.clone().unwrap_or_else(|| d.name.clone());
                out(&format!("{name} : ?"))?
            }
        }
    }
    if opts.only.is_some() && !selected {
        eprintln!(
            "no definition named `{}`",
            opts.only.as_deref().unwrap_or_default()
        );
        failed = true;
    }
    Ok(done(failed))
}

/// The default mode: the SQL for a file's entry point, which is the definition
/// named `main`.
///
/// A file is a *program*, not a bag of queries: it defines helpers, and `main`
/// says which one is the answer. Printing every root definition meant a module
/// of table declarations — `lib/icbase` is 737 of them — dumped its whole
/// schema as SQL, and a file that was being read for its one entry point
/// buried it among the helpers.
///
/// So a file with no `main` prints nothing, and that is a normal outcome
/// rather than an error: a library is a file with no entry point. It is still
/// loaded, checked and reported on, so a mistake in it is not hidden by its
/// silence. `--only` names a different definition, for reading one helper.
fn report_sql(
    ws: &Workspace,
    tc: &cagara_hir::TypeCheck,
    opts: &Options,
    mut failed: bool,
) -> std::io::Result<ExitCode> {
    let mut printed = false;
    // The entry point, unless `--only` named another definition.
    let wanted = opts.only.as_deref().unwrap_or(ENTRY_POINT);
    // One failing helper is reported both as itself and through every query
    // that uses it, so the same diagnostic can arrive more than once. Print
    // each distinct one once, in the order it first appears.
    let mut seen: HashSet<String> = HashSet::new();
    let compiled = compile_checked(ws, tc);
    // Diagnostics are rendered against the path the user named, not the
    // canonical one the workspace loaded under: a run from the file's own
    // directory would otherwise print the whole absolute path back at someone
    // who typed `cagara q.cagara`.
    let shown = opts.file.display().to_string();
    let load_path = ws.modules[ws.root].path.display().to_string();
    let display = |d: &cagara_hir::workspace::Diag| rebase_path(d, &load_path, &shown);
    // The entry point is a query, not a scalar. Without this check a scalar
    // `main` compiled cleanly and printed nothing, which looks like success.
    // `--only` is an explicit request for another definition, so it can still
    // inspect a scalar helper in such a file.
    if opts.only.is_none() && !opts.list {
        if let Some(ty) = main_that_is_not_a_query(ws, tc) {
            eprintln!(
                "{shown}: error: `main` must be a query, found `{ty}`; a file without a query \
                 entry point should omit `main` to be a library"
            );
            failed = true;
        }
    }
    // Every root definition that failed is reported, whether or not it is the
    // one being printed. A file is checked as a whole even when one definition
    // is the answer: a broken helper is a broken program, and reporting only
    // what was printed would let a library fail in silence — which is exactly
    // what printing nothing must not come to mean.
    for query in &compiled.queries {
        if let Err(d) = &query.result {
            let e = if query.name == wanted {
                display(d)
            } else {
                format!("{}: error in `{}`: {}", shown, query.name, display(d))
            };
            failed = true;
            if seen.insert(e.clone()) {
                eprintln!("{e}");
            }
        }
    }
    for query in compiled.queries {
        let name = query.name;
        if !opts.list && name != wanted {
            continue;
        }
        // Already reported above, in the same words.
        let Ok(rel) = query.result else {
            continue;
        };
        match cagara_sql::compile_located(
            &rel,
            cagara_sql::Options {
                dialect: opts.dialect,
                pretty: opts.pretty,
                optimize: opts.optimize,
            },
        )
        .map_err(|(loc, m)| {
            // A lowering refusal names the stage that caused it, when the IR still
            // remembers where that was. `diag_span` is the same conversion the
            // checker's diagnostics go through, so the line and column are measured
            // against the same text — and a `Loc` in an imported module resolves
            // against that module's file, not this one.
            match loc {
                Some(l) => format!(
                    "{}",
                    ws.diag_span(l.module, l.span, format!("error in `{name}`: {m}"))
                ),
                None => format!("{shown}: error in `{name}`: {m}"),
            }
        }) {
            Ok(sql) => {
                // One statement per run: `main`, or the one `--only` named.
                // Nothing separates it from anything, because there is nothing
                // else — the blank line this used to print between queries has
                // no second query to sit between.
                if printed {
                    out("")?;
                }
                let label = opts.label.clone().unwrap_or_else(|| name.clone());
                out(&format!("-- {label}\n{sql};"))?;
                printed = true;
            }
            Err(e) => {
                failed = true;
                if seen.insert(e.clone()) {
                    eprintln!("{e}");
                }
            }
        }
    }
    // A file without a `main` is a library and says nothing. Asking for a
    // definition by name is a request that can be missed, so that is reported.
    if let (Some(o), false, false) = (&opts.only, printed, failed) {
        if o == ARG_ENTRY {
            // The wrapper exists but is not a query: either the definition
            // takes arguments that were not supplied, or it is a helper.
            let name = opts.label.clone().unwrap_or_else(|| o.clone());
            let arity = ws.modules[ws.root]
                .module
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.name == ARG_ENTRY)
                .and_then(|(i, _)| tc.type_of(ws.root, i))
                .map(|t| t.matches("->").count())
                .unwrap_or(0);
            if arity > 0 {
                eprintln!(
                    "`{name}` is a function of {arity} argument(s), so it is not a query: pass \
                     them with `--arg VALUE` (repeatable)"
                );
            } else {
                eprintln!("`{name}` is not a query");
            }
        } else {
            eprintln!("no query definition named `{o}`");
        }
        failed = true;
    }
    Ok(done(failed))
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n{USAGE}");
    ExitCode::from(2)
}

/// A diagnostic rendered against the path the user typed.
///
/// The workspace canonicalizes paths when it loads them, so a diagnostic
/// carries the absolute one. Replacing the loaded prefix with the name as
/// typed keeps the rest of the rendering (`:line:col`, excerpt) untouched.
fn rebase_path(d: &cagara_hir::workspace::Diag, loaded: &str, shown: &str) -> String {
    let rendered = d.to_string();
    if loaded == shown || loaded.is_empty() {
        return rendered;
    }
    match rendered.strip_prefix(loaded) {
        Some(rest) => format!("{shown}{rest}"),
        None => rendered,
    }
}

fn display(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

/// One line to stdout. Errors — a closed reader (`| head`) included —
/// propagate to `main`, which turns `BrokenPipe` into a clean exit.
pub(crate) fn out(s: &str) -> std::io::Result<()> {
    let mut o = std::io::stdout().lock();
    o.write_all(s.as_bytes())?;
    o.write_all(b"\n")
}

pub(crate) fn done(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
