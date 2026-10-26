//! Stamps the build into the binary: the commit, the profile, the target and
//! the time.
//!
//! A `cagara` binary is copied around — staged into `editors/vscode/bin/`, put
//! in a VSIX, dropped into the extension's install directory — and once it has
//! been copied, nothing on the outside says where it came from. A copy carries
//! the copy's timestamp, so an old build looks new exactly when it matters.
//! The language server names this stamp in its first log line, which is the one
//! place a window can say which compiler is actually answering.
//!
//! Nothing here may fail the build: `git` is absent in a tarball, and a build
//! script that errors takes the whole build with it. Every value falls back to
//! `unknown`.

use std::env;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    // Two directories up: `crates/cagara-cli` → the workspace root, where the
    // repository's git state lives.
    let root = Path::new(&manifest).join("..").join("..");

    // Cargo reruns a build script only when a file it names changes. The commit
    // is in no file of this package, so name the git state that holds it;
    // otherwise the first build's commit is reported by every later one.
    if let Some(git_dir) = git(&root, &["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/refs");
    }

    println!("cargo:rustc-env=CAGARA_BUILD_COMMIT={}", commit(&root));
    // Cargo sets these for the build script; they describe the compilation that
    // is about to embed the stamp, not the machine running the script.
    println!(
        "cargo:rustc-env=CAGARA_BUILD_PROFILE={}",
        env::var("PROFILE").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "cargo:rustc-env=CAGARA_BUILD_TARGET={}",
        env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!("cargo:rustc-env=CAGARA_BUILD_DATE={}", date());
}

/// The commit this build came from, `+dirty` when the tree it was built in had
/// uncommitted changes to tracked files.
///
/// Untracked files are ignored: a stray file changes the tree but not the code
/// that was compiled, and marking every build dirty for it would make the
/// marker useless.
fn commit(root: &Path) -> String {
    let Some(sha) = git(root, &["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".into();
    };
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|changes| !changes.is_empty());
    if dirty {
        format!("{sha}+dirty")
    } else {
        sha
    }
}

/// One `git` invocation, `None` when git is missing, the directory is not a
/// repository, or the command fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `YYYY-MM-DD HH:MM UTC`, from the clock or from `SOURCE_DATE_EPOCH` when a
/// reproducible build sets it.
fn date() -> String {
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });
    let (year, month, day, hour, minute) = civil(secs);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Seconds since the epoch as a UTC calendar date and time of day, by Howard
/// Hinnant's `civil_from_days`.
///
/// Written out rather than pulled in: this is one log line, and a date crate
/// for it would be a dependency the lockfile and the release build both have to
/// carry.
fn civil(secs: i64) -> (i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
    // year and makes the month lengths a repeating pattern.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
    )
}

#[cfg(test)]
mod tests {
    use super::civil;

    /// A stamp that cannot be read back is worse than none: pin the epoch, a
    /// leap day, and the last second of a year.
    #[test]
    fn the_date_is_a_utc_calendar_date() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0));
        assert_eq!(civil(1_700_000_000), (2023, 11, 14, 22, 13));
        // 2024-02-29T23:59:00Z — the leap day, and the leap year's February.
        assert_eq!(civil(1_709_251_140), (2024, 2, 29, 23, 59));
        // 1970-01-01T00:00:00Z one second earlier is in 1969.
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59));
        assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0));
    }
}
