//! Diagnostics, hover, and definitions for the root file of a workspace.
//! LSP positions count UTF-16 code units; the workspace uses byte offsets.

use cagara_hir::check::{check, TypeCheck};
use cagara_hir::workspace::{Binding, Diag, Workspace};
use cagara_hir::root_queries_checked;
use lsp_types::{Position, Range};
use std::path::PathBuf;

/// Every diagnostic in the root file: loading (syntax, imports,
/// overloads), type errors, and evaluation / schema errors.
pub fn diagnostics(ws: &Workspace) -> Vec<(Range, String)> {
    let tc = check(ws);
    let root = ws.modules[ws.root].path.display().to_string();
    let mut all: Vec<Diag> = ws.diags.clone();
    all.extend(tc.errors.iter().map(|e| e.diag.clone()));
    all.extend(root_queries_checked(ws, &tc).into_iter().filter_map(|(_, r)| r.err()));
    let mut out: Vec<(Range, String)> = Vec::new();
    for d in all.iter().filter(|d| d.path == root) {
        let item = (diag_range(d), d.message.clone());
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Markdown for the name under the cursor: its inferred type (every
/// candidate for an overload set).
pub fn hover(ws: &Workspace, pos: Position) -> Option<String> {
    let tc = check(ws);
    let (name, b) = binding_at(ws, pos)?;
    let lines = match b {
        Binding::Def(m, i) => vec![def_line(&tc, &name, m, i)],
        Binding::Overloads(m, is) => is.iter().map(|&i| def_line(&tc, &name, m, i)).collect(),
        Binding::Module(t) => vec![format!("module {}", ws.modules[t].path.display())],
        Binding::Prim(_) => vec![format!("{name} : primitive")],
    };
    Some(format!("```cagara\n{}\n```", lines.join("\n")))
}

/// Where the name under the cursor is defined: file and range of the name
/// in each definition (several for an overload set). The prelude has no
/// file, so its definitions are not returned.
pub fn definition(ws: &Workspace, pos: Position) -> Vec<(PathBuf, Range)> {
    let Some((_, b)) = binding_at(ws, pos) else { return vec![] };
    let (m, is) = match b {
        Binding::Def(m, i) => (m, vec![i]),
        Binding::Overloads(m, is) => (m, is),
        _ => return vec![],
    };
    if m == 0 {
        return vec![];
    }
    let md = &ws.modules[m];
    is.iter()
        .map(|&i| {
            let d = &md.module.defs[i];
            let start = d.span.start as usize;
            let range = Range { start: position_of(&md.text, start), end: position_of(&md.text, start + d.name.len()) };
            (md.path.clone(), range)
        })
        .collect()
}

fn def_line(tc: &TypeCheck, name: &str, m: usize, i: usize) -> String {
    format!("{name} : {}", tc.type_of(m, i).unwrap_or("(type error)"))
}

/// The identifier under the cursor and what it refers to in the root
/// module: a name in scope, or `alias.name` of an imported module. Column
/// references (`.x`) have no binding.
fn binding_at(ws: &Workspace, pos: Position) -> Option<(String, Binding)> {
    let md = &ws.modules[ws.root];
    let text = &md.text;
    let (start, end) = ident_at(text, offset_at(text, pos)?)?;
    let name = &text[start..end];
    let b = if start > 0 && text.as_bytes()[start - 1] == b'.' {
        let (ps, pe) = ident_at(text, start - 1)?;
        match md.scope.get(&text[ps..pe])? {
            Binding::Module(t) => ws.modules[*t].own.get(name)?.clone(),
            _ => return None,
        }
    } else {
        md.scope.get(name)?.clone()
    };
    Some((name.to_string(), b))
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte range of the identifier touching `offset` (on either side).
fn ident_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut s = offset.min(b.len());
    while s > 0 && is_ident(b[s - 1]) {
        s -= 1;
    }
    let mut e = offset.min(b.len());
    while e < b.len() && is_ident(b[e]) {
        e += 1;
    }
    (s < e && !b[s].is_ascii_digit()).then_some((s, e))
}

/// Byte offset of an LSP position (UTF-16 columns).
pub fn offset_at(text: &str, pos: Position) -> Option<usize> {
    let mut line_start = 0;
    for _ in 0..pos.line {
        line_start += text[line_start..].find('\n')? + 1;
    }
    let line_end = text[line_start..].find('\n').map_or(text.len(), |i| line_start + i);
    let mut units = 0;
    for (i, c) in text[line_start..line_end].char_indices() {
        if units >= pos.character {
            return Some(line_start + i);
        }
        units += c.len_utf16() as u32;
    }
    Some(line_end)
}

/// LSP position of a byte offset.
pub fn position_of(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position { line: before.matches('\n').count() as u32, character: before[line_start..].encode_utf16().count() as u32 }
}

/// A diagnostic's range from its line, byte column, and width in chars.
fn diag_range(d: &Diag) -> Range {
    let line = d.line.saturating_sub(1) as u32;
    let col = d.col.saturating_sub(1);
    let start = d.source.get(..col).map_or(col, |s| s.encode_utf16().count()) as u32;
    let width = d.source.get(col..).map_or(1, |s| s.chars().take(d.width.max(1)).map(char::len_utf16).sum::<usize>().max(1)) as u32;
    Range { start: Position { line, character: start }, end: Position { line, character: start + width } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SRC: &str = "users : query { id = int, name = string, age = int } = table \"p\" \"users\"\n\
                       adult = .age >= 18\n\
                       q = users & where adult & agg { s = sum .age }\n\
                       bad = users & select { x = \"é\" <> .age }\n";

    fn ws() -> Workspace {
        Workspace::open_with(Path::new("/nonexistent/main.cagara"), SRC.to_string())
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn diagnostics_have_utf16_ranges() {
        let ds = diagnostics(&ws());
        assert_eq!(ds.len(), 1, "{ds:?}");
        let (r, msg) = &ds[0];
        assert!(msg.contains("type mismatch"), "{msg}");
        assert_eq!(r.start.line, 3);
        // The span starts after `"é" <> ` or at the argument; either way the
        // column counts `é` as one UTF-16 unit, not two bytes.
        let line = SRC.lines().nth(3).unwrap();
        let byte = offset_at(SRC, r.start).unwrap() - SRC.find(line).unwrap();
        assert!(line.is_char_boundary(byte), "{r:?}");
    }

    #[test]
    fn hover_shows_types_and_overloads() {
        let ws = ws();
        // `adult` in `where adult` (line 2, column 18).
        let h = hover(&ws, pos(2, 19)).unwrap();
        assert!(h.contains("adult : expr { age = a | b } bool"), "{h}");
        // `sum` is an overload set in the prelude.
        let h = hover(&ws, pos(2, 37)).unwrap();
        assert_eq!(h.matches("sum : ").count(), 4, "{h}");
        // A column reference has no binding.
        assert!(hover(&ws, pos(1, 10)).is_none());
    }

    #[test]
    fn definition_points_at_the_name() {
        let ws = ws();
        let d = definition(&ws, pos(2, 19));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, Range { start: pos(1, 0), end: pos(1, 5) });
        // Prelude definitions have no file.
        assert!(definition(&ws, pos(2, 37)).is_empty());
    }

    #[test]
    fn positions_round_trip() {
        let t = "ab\né x\n";
        for off in [0, 1, 3, 5, 6] {
            assert_eq!(offset_at(t, position_of(t, off)), Some(off));
        }
        assert_eq!(position_of(t, 6), pos(1, 2));
    }
}
