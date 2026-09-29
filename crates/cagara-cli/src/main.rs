//! `cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]`
//! `cagara fmt [--check] <files...|->`
//! `cagara lsp`
//!
//! Prints one SQL statement per query definition in the root file, or with
//! `--types` the inferred type of every root definition. `cagara fmt`
//! formats files in place. `cagara lsp` runs the language server over stdio.

use cagara_hir::{check, root_queries_checked, Workspace};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]
       cagara fmt [--check] <files...>   format files in place (`-` for stdin to stdout)
       cagara lsp    run the language server over stdio";

fn main() -> ExitCode {
    let mut rest = std::env::args().skip(1);
    if std::env::args().nth(1).as_deref() == Some("fmt") {
        rest.next();
        return fmt(rest);
    }
    if std::env::args().nth(1).as_deref() == Some("lsp") {
        rest.next();
        // Editors may pass `--stdio`; stdio is the only transport.
        if let Some(a) = rest.find(|a| a != "--stdio") {
            return usage(&format!("unexpected argument `{a}` for `cagara lsp`"));
        }
        return match cagara_lsp::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("cagara lsp: {e}");
                ExitCode::FAILURE
            }
        };
    }
    compile(rest)
}

fn compile(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut file: Option<PathBuf> = None;
    let mut dialect_name = String::from("ansi");
    let mut only: Option<String> = None;
    let mut pretty = false;
    let mut types = false;
    let mut optimize = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--dialect" => match args.next() {
                Some(d) => dialect_name = d,
                None => return usage("--dialect needs a value"),
            },
            "--only" => match args.next() {
                Some(d) => only = Some(d),
                None => return usage("--only needs a definition name"),
            },
            "--pretty" => pretty = true,
            "--types" => types = true,
            "--optimize" => optimize = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            f if file.is_none() && !f.starts_with("--") => file = Some(PathBuf::from(f)),
            other => return usage(&format!("unexpected argument `{other}`")),
        }
    }
    let Some(file) = file else { return usage("missing input file") };
    let Some(dialect) = cagara_sql::dialect(&dialect_name) else {
        return usage(&format!("unknown dialect `{dialect_name}`"));
    };

    let ws = Workspace::open(&file);
    if !ws.diags.is_empty() {
        for d in &ws.diags {
            eprintln!("{d}");
        }
        return ExitCode::FAILURE;
    }

    // Errors in imported modules are always reported; root errors are
    // reported per definition below (so `--only` applies to them).
    let tc = check(&ws);
    let mut failed = false;
    for e in tc.errors.iter().filter(|e| e.module != ws.root) {
        eprintln!("{}", e.diag);
        failed = true;
    }

    if types {
        for (i, d) in ws.modules[ws.root].module.defs.iter().enumerate() {
            if only.as_deref().is_some_and(|o| o != d.name) {
                continue;
            }
            match (tc.error_for(ws.root, i), tc.type_of(ws.root, i)) {
                (Some(e), _) => {
                    eprintln!("{e}");
                    failed = true;
                }
                (None, Some(t)) => println!("{} : {t}", d.name),
                (None, None) => println!("{} : ?", d.name),
            }
        }
        return if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut printed = 0;
    for (name, result) in root_queries_checked(&ws, &tc) {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        match result.map_err(|d| d.to_string()).and_then(|rel| {
            cagara_sql::compile(&rel, cagara_sql::Options { dialect, pretty, optimize }).map_err(|m| format!("{}: error in `{name}`: {m}", file.display()))
        }) {
            Ok(sql) => {
                if printed > 0 {
                    println!();
                }
                println!("-- {name}\n{sql};");
                printed += 1;
            }
            Err(e) => {
                eprintln!("{e}");
                failed = true;
            }
        }
    }
    if let (Some(o), 0, false) = (&only, printed, failed) {
        eprintln!("no query definition named `{o}`");
        failed = true;
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// `cagara fmt`: rewrite files in place, or with `--check` list the files
/// that are not formatted. Files with syntax errors are reported and left
/// unchanged. `-` formats stdin to stdout (echoing it on failure, so editor
/// filters never lose text).
fn fmt(args: impl Iterator<Item = String>) -> ExitCode {
    let mut check = false;
    let mut files = Vec::new();
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-" => files.push(a),
            f if !f.starts_with('-') => files.push(a),
            other => return usage(&format!("unexpected argument `{other}` for `cagara fmt`")),
        }
    }
    if files.is_empty() {
        return usage("`cagara fmt` needs files to format (or `-` for stdin)");
    }
    let mut failed = false;
    for file in &files {
        let stdin = file == "-";
        let name = if stdin { "<stdin>" } else { file.as_str() };
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
        let out = match cagara_fmt::format(&src) {
            Ok(f) if f.errors.is_empty() => Some(f.text),
            Ok(f) => {
                for e in &f.errors {
                    let (line, col) = cagara_fmt::line_col(&src, e.offset);
                    eprintln!("{name}:{line}:{col}: error: {} (file not formatted)", e.message);
                }
                None
            }
            Err(e) => {
                eprintln!("{name}: {e}");
                None
            }
        };
        failed |= out.is_none();
        if stdin {
            let text = if check { None } else { Some(out.as_deref().unwrap_or(&src)) };
            if let Some(t) = text {
                if std::io::stdout().write_all(t.as_bytes()).is_err() {
                    return ExitCode::FAILURE;
                }
            }
        }
        let Some(out) = out else { continue };
        if out == src {
            continue;
        }
        if check {
            println!("{name}");
            failed = true;
        } else if !stdin {
            if let Err(e) = std::fs::write(file, out) {
                eprintln!("{name}: {e}");
                failed = true;
            }
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n{USAGE}");
    ExitCode::from(2)
}
