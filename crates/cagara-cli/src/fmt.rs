//! `cagara fmt`: rewrite files in place, or with `--check` list the files that
//! are not formatted.
//!
//! Files with syntax errors are reported and left unchanged. `-` formats stdin
//! to stdout (echoing it on failure, so editor filters never lose text). Output
//! uses LF line endings; a leading UTF-8 BOM is preserved.

use crate::{display, done, out, usage, USAGE};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> std::io::Result<ExitCode> {
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
