//! Opt-in scale measurements; each invocation benchmarks one fresh workspace.

use super::*;
use std::fmt::Write;
use std::time::Instant;

fn timed<T>(phase: &str, work: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = work();
    println!("{phase}: {:?}", start.elapsed());
    value
}

fn memory(phase: &str) {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status
            .lines()
            .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
        {
            println!("{phase} {line}");
        }
    }
}

#[test]
#[ignore = "scale benchmark; set CAGARA_BENCH_DEFINITIONS and CAGARA_BENCH_LAYOUT=root|import"]
fn large_definition_cache_benchmark() {
    let count = std::env::var("CAGARA_BENCH_DEFINITIONS")
        .map(|value| value.parse::<usize>().expect("positive definition count"))
        .unwrap_or(100_000);
    assert!(count >= 2, "benchmark needs at least two definitions");
    let layout = std::env::var("CAGARA_BENCH_LAYOUT").unwrap_or_else(|_| "root".into());
    assert!(matches!(layout.as_str(), "root" | "import"));
    println!("layout={layout}, definitions={count}");

    let mut source = String::with_capacity(count.saturating_mul(64));
    for i in 0..count {
        writeln!(
            source,
            "d{i} : query {{ a = int }} = table \"public\" \"t{i}\""
        )
        .unwrap();
    }
    let mut ws = timed("load", || {
        if layout == "root" {
            Workspace::from_source(&source)
        } else {
            let path = Path::new("/tmp/cagara-scale/root.cagara");
            let root = "import \"catalog.cagara\" as catalog\nq = catalog.d0\n";
            let buffers = HashMap::from([
                (path.to_path_buf(), root.to_string()),
                (
                    path.with_file_name("catalog.cagara"),
                    std::mem::take(&mut source),
                ),
            ]);
            Workspace::open_with_buffers(path, root.into(), &buffers)
        }
    });
    drop(source);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);

    let tc = timed("cold_check", || crate::check::check(&ws));
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    drop(tc);
    let first = timed("cold_compile_owned", || compile(&ws));
    assert!(first.is_ok(), "{:?}", first.diagnostics);
    assert_eq!(
        first.queries.len(),
        if layout == "root" { count } else { 1 }
    );
    drop(first);
    memory("cold");

    let before = crate::elaborate::elaborated_defs().len();
    let start = Instant::now();
    for _ in 0..100 {
        assert!(crate::check::check(&ws).errors().is_empty());
    }
    println!("warm_check_per_request: {:?}", start.elapsed() / 100);
    let start = Instant::now();
    for _ in 0..100 {
        assert!(compile_diagnostics(&ws).is_empty());
    }
    println!("warm_diagnostics_per_request: {:?}", start.elapsed() / 100);
    assert_eq!(crate::elaborate::elaborated_defs().len(), before);

    let root_text = &ws.modules[ws.root].text;
    let edit = if layout == "root" {
        root_text.replacen("\"t0\"", "\"tx\"", 1)
    } else {
        root_text.replace("catalog.d0", "catalog.d1")
    };
    assert!(timed("set_source", || ws.set_source(ws.root, edit)));
    let tc = timed("edited_check", || crate::check::check(&ws));
    assert!(tc.errors().is_empty(), "{:?}", tc.errors());
    drop(tc);
    let second = timed("edited_compile_owned", || compile(&ws));
    assert!(second.is_ok(), "{:?}", second.diagnostics);
    assert_eq!(
        second.queries.len(),
        if layout == "root" { count } else { 1 }
    );
    assert_eq!(
        crate::elaborate::elaborated_defs().len() - before,
        1,
        "one-definition edits should elaborate one root query"
    );
    drop(second);
    memory("edited");
    timed("drop_workspace", || drop(ws));
    memory("finished");
}
