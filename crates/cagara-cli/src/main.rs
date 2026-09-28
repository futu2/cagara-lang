//! `cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty]`
//!
//! Prints one SQL statement per query definition in the root file.

use cagara_hir::{root_queries, Workspace};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cagara <file.cagara> [--dialect NAME] [--only DEF] [--pretty]";

fn main() -> ExitCode {
    let mut file: Option<PathBuf> = None;
    let mut dialect_name = String::from("ansi");
    let mut only: Option<String> = None;
    let mut pretty = false;

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

    let mut failed = false;
    let mut printed = 0;
    for (name, result) in root_queries(&ws) {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        match result.map_err(|d| d.to_string()).and_then(|rel| {
            cagara_sql::compile(&rel, dialect, pretty).map_err(|m| format!("{}: error in `{name}`: {m}", file.display()))
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
