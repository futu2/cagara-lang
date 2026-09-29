//! `cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]`
//!
//! Prints one SQL statement per query definition in the root file, or with
//! `--types` the inferred type of every root definition.

use cagara_hir::{check, root_queries_checked, Workspace};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty] [--optimize] [--types]";

fn main() -> ExitCode {
    let mut file: Option<PathBuf> = None;
    let mut dialect_name = String::from("ansi");
    let mut only: Option<String> = None;
    let mut pretty = false;
    let mut types = false;
    let mut optimize = false;

    let mut args = std::env::args().skip(1);
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

fn usage(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n{USAGE}");
    ExitCode::from(2)
}
