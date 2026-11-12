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

mod doc;

use cagara_hir::check::SchemeView;
use cagara_hir::{check, compile_checked, TypeCheck, Workspace};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
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
        return fmt(rest);
    }
    if first.as_deref() == Some(OsStr::new("doc")) {
        return doc::run(rest);
    }
    if first.as_deref() == Some(OsStr::new("check")) {
        return check_tree(rest);
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
    let tc = check(ws);
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
    // Parameters first: the wrapper calls a definition, and that definition may
    // read one. Cagara resolves forward references, so the order is for reading.
    text.push_str(params);
    text.push_str(&format!("\n{ARG_ENTRY} = {body}\n"));
    Ok(Workspace::open_with(path, text))
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
        match cagara_sql::compile(
            &rel,
            cagara_sql::Options {
                dialect: opts.dialect,
                pretty: opts.pretty,
                optimize: opts.optimize,
            },
        )
        .map_err(|m| format!("{shown}: error in `{name}`: {m}"))
        {
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

/// `cagara fmt`: rewrite files in place, or with `--check` list the files
/// that are not formatted. Files with syntax errors are reported and left
/// unchanged. `-` formats stdin to stdout (echoing it on failure, so editor
/// filters never lose text). Output uses LF line endings; a leading UTF-8 BOM
/// is preserved.
fn fmt(args: impl Iterator<Item = OsString>) -> std::io::Result<ExitCode> {
    let mut check = false;
    let mut files = Vec::new();
    for a in args {
        match a.to_str() {
            Some("--check") => check = true,
            Some("-h") | Some("--help") => {
                out(USAGE)?;
                return Ok(ExitCode::SUCCESS);
            }
            Some("-") => files.push(a),
            _ if !a.to_string_lossy().starts_with('-') => files.push(a),
            _ => {
                return Ok(usage(&format!(
                    "unexpected argument `{}` for `cagara fmt`",
                    display(&a)
                )))
            }
        }
    }
    if files.is_empty() {
        return Ok(usage(
            "`cagara fmt` needs files to format (or `-` for stdin)",
        ));
    }
    let mut failed = false;
    for file in &files {
        let stdin = file == OsStr::new("-");
        let displayed = display(file);
        let name = if stdin { "<stdin>" } else { displayed.as_str() };
        let read = if stdin {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map(|_| s)
        } else {
            std::fs::read_to_string(file)
        };
        let src = match read {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{name}: {e}");
                failed = true;
                continue;
            }
        };
        // A BOM is transport metadata. Format the body, then put the marker
        // back so formatting a Windows file does not silently change its
        // encoding.
        let has_bom = src.starts_with('\u{feff}');
        let body = cagara_syntax::strip_bom(&src);
        let formatted = match cagara_fmt::format(body) {
            Ok(f) if f.errors.is_empty() => Some(if has_bom {
                format!("\u{feff}{}", f.text)
            } else {
                f.text
            }),
            Ok(f) => {
                for e in &f.errors {
                    let (line, col) = cagara_fmt::line_col(body, e.offset);
                    eprintln!(
                        "{name}:{line}:{col}: error: {} (file not formatted)",
                        e.message
                    );
                }
                None
            }
            Err(e) => {
                eprintln!("{name}: {e}");
                None
            }
        };
        failed |= formatted.is_none();
        if stdin {
            let text = if check {
                None
            } else {
                Some(formatted.as_deref().unwrap_or(&src))
            };
            if let Some(t) = text {
                std::io::stdout().write_all(t.as_bytes())?;
            }
        }
        let Some(formatted) = formatted else { continue };
        if formatted == src {
            continue;
        }
        if check {
            out(name)?;
            failed = true;
        } else if !stdin {
            if let Err(e) = write_atomic(Path::new(file), formatted.as_bytes()) {
                eprintln!("{name}: {e}");
                failed = true;
            }
        }
    }
    Ok(done(failed))
}

/// Replace `path` with `contents`, writing a sibling temporary first.
///
/// A crash during a plain `fs::write` can leave a truncated source file. The
/// rename is atomic on Unix; Windows has no portable rename-over-existing, so
/// the old file is removed first there. The file's permissions are copied to
/// the temporary so replacing it does not change its mode.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // Follow a symlink, so formatting a link updates its target rather than
    // replacing the link with a regular file.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut last = None;
    for attempt in 0..100_u32 {
        let tmp = parent.join(format!(".cagara-fmt-{}-{attempt}", std::process::id()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = (|| {
            if let Ok(meta) = fs::metadata(&target) {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            #[cfg(windows)]
            if target.exists() {
                fs::remove_file(&target)?;
            }
            fs::rename(&tmp, &target)
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::other(format!(
            "could not create a temporary file next to {}",
            path.display()
        ))
    }))
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

// ── `cagara check` ───────────────────────────────────────────────────────────
//
// One answer for a tree of files: which of them do not check, and why. The
// single-file path above answers "what SQL does this program mean"; this
// answers "is this directory sound", which is what a pre-commit hook and an
// editor's project mode need. It compiles nothing to SQL: a file is checked as
// a *library*, so a directory of table declarations is reportable without one
// of them being `main`.

/// A diagnostic in the form `check` prints.
struct Found {
    /// `error` or `warning`: only the former fails the run.
    severity: &'static str,
    path: String,
    line: usize,
    col: usize,
    message: String,
}

impl Found {
    fn text(&self) -> String {
        format!(
            "{}:{}:{}: {}: {}",
            self.path, self.line, self.col, self.severity, self.message
        )
    }

    /// Whether this diagnostic fails the run. A warning is reported and the
    /// run succeeds: the program compiles and means what its types say.
    fn fails(&self) -> bool {
        self.severity == "error"
    }

    /// One JSON object per line, so a consumer can read the stream without a
    /// parser for the whole document, and a truncated run still yields the
    /// diagnostics it got to.
    fn json(&self) -> String {
        fn esc(s: &str) -> String {
            let mut o = String::with_capacity(s.len() + 2);
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    '\r' => o.push_str("\\r"),
                    '\t' => o.push_str("\\t"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o
        }
        format!(
            "{{\"severity\":\"{}\",\"path\":\"{}\",\"line\":{},\"column\":{},\"message\":\"{}\"}}",
            self.severity,
            esc(&self.path),
            self.line,
            self.col,
            esc(&self.message)
        )
    }
}

/// Every `*.cagara` file under `path`, in a stable order. A file path is itself.
fn sources_under(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !path.is_dir() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                // A dot directory is a build artifact or an editor's cache,
                // not part of the tree being checked.
                if entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                stack.push(entry);
            } else if entry.extension().is_some_and(|e| e == "cagara") {
                out.push(entry);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `cagara check [--json] <path>...`: report every file that does not check.
///
/// Exit codes: `0` all files check, `1` at least one diagnostic, `2` a usage
/// error. The distinction is what lets a hook say "this is broken" and "you
/// called me wrong" differently.
fn check_tree(args: impl Iterator<Item = OsString>) -> std::io::Result<ExitCode> {
    let mut json = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for a in args {
        match a.to_str() {
            Some("--json") => json = true,
            Some("-h") | Some("--help") => {
                out(USAGE)?;
                return Ok(ExitCode::SUCCESS);
            }
            _ if a.to_string_lossy().starts_with("--") => {
                return Ok(usage(&format!(
                    "unexpected argument `{}` for `cagara check`",
                    display(&a)
                )))
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        return Ok(usage("`cagara check` needs a file or a directory"));
    }
    let mut files = Vec::new();
    for p in &paths {
        match sources_under(p) {
            Ok(found) => files.extend(found),
            Err(e) => {
                eprintln!("{}: {e}", p.display());
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    let mut failed = false;
    for file in &files {
        let ws = Workspace::open(file);
        let mut found: Vec<Found> = Vec::new();
        for d in &ws.diags {
            found.push(Found {
                severity: d.severity.as_str(),
                path: d.path.clone(),
                line: d.line,
                col: d.col,
                message: d.message.clone(),
            });
        }
        if ws.diags.is_empty() {
            let tc = check(&ws);
            for e in tc.errors() {
                found.push(Found {
                    severity: e.diag.severity.as_str(),
                    path: e.diag.path.clone(),
                    line: e.diag.line,
                    col: e.diag.col,
                    message: e.diag.message.clone(),
                });
            }
            for w in tc.warnings() {
                found.push(Found {
                    severity: w.diag.severity.as_str(),
                    path: w.diag.path.clone(),
                    line: w.diag.line,
                    col: w.diag.col,
                    message: w.diag.message.clone(),
                });
            }
        }
        for f in found {
            if json {
                out(&f.json())?;
            } else {
                eprintln!("{}", f.text());
            }
            failed |= f.fails();
        }
    }
    Ok(done(failed))
}
