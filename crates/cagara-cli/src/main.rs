//! `cagara <file.cagara|-> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]`
//! `cagara fmt [--check] <files...|->`
//! `cagara lsp`
//!
//! Prints the SQL for the root file's `main` definition, or with `--only`
//! another definition; `--types` prints inferred types instead. A file without
//! `main` is a library and prints nothing. `cagara fmt` formats files in place.
//! `cagara lsp` runs the language server over stdio.

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

const USAGE: &str =
    "usage: cagara <file.cagara|-> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
       cagara fmt [--check] <files...>   format files in place (`-` for stdin to stdout)
       cagara lsp    run the language server over stdio
       cagara --version";

/// The path a source read from stdin is reported as, so a diagnostic still
/// names a file. A real file of that name is not consulted: `open_source`
/// takes the text directly.
const STDIN_PATH: &str = "<stdin>";

/// The definition a file is compiled for, unless `--only` names another.
///
/// `main` is the entry point by convention, and the only definition whose SQL
/// is printed by default. A file that does not define one prints nothing.
const ENTRY_POINT: &str = "main";

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
        return match cagara_lsp::run() {
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

    let ws = match open_source(&opts.file) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("{STDIN_PATH}: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    if !ws.diags.is_empty() {
        for d in &ws.diags {
            eprintln!("{d}");
        }
        return Ok(ExitCode::FAILURE);
    }

    // Errors in imported modules are always reported; root errors are
    // reported per definition below (so `--only` applies to them).
    let tc = check(&ws);
    let mut failed = false;
    for e in tc.errors().iter().filter(|e| e.module != ws.root) {
        eprintln!("{}", e.diag);
        failed = true;
    }

    if opts.types {
        report_types(&ws, &tc, &opts, failed)
    } else {
        report_sql(&ws, &tc, &opts, failed)
    }
}

/// Open `path` as a workspace, reading stdin when it is [`STDIN_PATH`].
///
/// `Workspace::open` reads from disk, so stdin is read here and handed to
/// `open_with` — which keeps imports resolving relative to the current
/// directory, exactly as `from_source` does.
fn open_source(path: &Path) -> std::io::Result<Workspace> {
    if path != Path::new(STDIN_PATH) {
        return Ok(Workspace::open(path));
    }
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(Workspace::open_with(path, text))
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
            (None, Some(t)) => out(&format!("{} : {t}", d.name))?,
            (None, None) => out(&format!("{} : ?", d.name))?,
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
    if opts.only.is_none() {
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
        if name != wanted {
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
                out(&format!("-- {name}\n{sql};"))?;
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
        eprintln!("no query definition named `{o}`");
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
fn out(s: &str) -> std::io::Result<()> {
    let mut o = std::io::stdout().lock();
    o.write_all(s.as_bytes())?;
    o.write_all(b"\n")
}

fn done(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
