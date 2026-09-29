//! Diagnostics, hover, definitions, references, highlights, symbols,
//! completion, and formatting for the root file of a workspace. Names are resolved on the
//! AST, so a lambda parameter shadows a top-level name of the same spelling.
//! LSP positions count UTF-16 code units; the workspace uses byte offsets.

use cagara_hir::check::{check, TypeCheck, PROBE_FIELD};
use cagara_hir::root_queries_checked;
use cagara_hir::workspace::{Binding, Diag, Workspace};
use cagara_syntax::ast::{Expr, ExprKind, Span};
use lsp_types::{
    CompletionItem, CompletionItemKind, DocumentHighlightKind, DocumentSymbol, Position, Range,
    SymbolKind, TextEdit,
};
use std::collections::HashSet;
use std::path::PathBuf;

/// Every diagnostic in the root file: loading (syntax, imports,
/// overloads), type errors, and evaluation / schema errors.
pub fn diagnostics(ws: &Workspace) -> Vec<(Range, String)> {
    let tc = check(ws);
    let root = ws.modules[ws.root].path.display().to_string();
    let mut all: Vec<Diag> = ws.diags.clone();
    all.extend(tc.errors.iter().map(|e| e.diag.clone()));
    all.extend(
        root_queries_checked(ws, &tc)
            .into_iter()
            .filter_map(|(_, r)| r.err()),
    );
    let mut out: Vec<(Range, String)> = Vec::new();
    for d in all.iter().filter(|d| d.path == root) {
        let item = (diag_range(d), d.message.clone());
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

// ── name resolution ──────────────────────────────────────────

/// What a name refers to.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Global(Binding),
    /// A lambda parameter, identified by the byte offset where it is bound.
    Local(usize),
}

/// One spelled occurrence of a name in the root file.
#[derive(Debug, Clone)]
struct Occ {
    start: usize,
    end: usize,
    target: Target,
    /// A binding site: a definition's name or a lambda parameter.
    decl: bool,
    /// For a lambda parameter: the end of the lambda (its scope).
    scope_end: usize,
    /// For a use: the span of its expression, which keys its instance type.
    site: Option<Span>,
}

/// Every resolved name occurrence in the root file, in source order.
/// Desugared names that are not spelled in the source (`negate` for `-x`)
/// are skipped; operators count where their symbol is written.
fn occurrences(ws: &Workspace) -> Vec<Occ> {
    let md = &ws.modules[ws.root];
    let mut w = Walk {
        ws,
        out: Vec::new(),
        env: Vec::new(),
    };
    for d in &md.module.defs {
        let start = d.span.start as usize;
        let spelled = md.text.get(start..).is_some_and(|t| t.starts_with(&d.name));
        if let (true, Some(b)) = (spelled, md.scope.get(&d.name)) {
            let end = start + d.name.len();
            let target = Target::Global(b.clone());
            w.out.push(Occ {
                start,
                end,
                target,
                decl: true,
                scope_end: end,
                site: None,
            });
        }
        w.expr(&d.body);
    }
    w.out
}

struct Walk<'a> {
    ws: &'a Workspace,
    out: Vec<Occ>,
    /// Lambda parameters in scope, innermost last: (name, binding offset).
    env: Vec<(String, usize)>,
}

impl Walk<'_> {
    fn expr(&mut self, e: &Expr) {
        let ws = self.ws;
        let md = &ws.modules[ws.root];
        match &e.kind {
            ExprKind::Name(n) => {
                let Some((start, end)) = spelled(&md.text, e.span, n) else {
                    return;
                };
                let target = match self.env.iter().rev().find(|(p, _)| p == n) {
                    Some(&(_, at)) => Target::Local(at),
                    None => match md.scope.get(n) {
                        Some(b) => Target::Global(b.clone()),
                        None => return,
                    },
                };
                self.out.push(Occ {
                    start,
                    end,
                    target,
                    decl: false,
                    scope_end: end,
                    site: Some(e.span),
                });
            }
            ExprKind::Proj(inner, field) => {
                self.expr(inner);
                // `alias.name`: the field resolves in the imported module.
                let ExprKind::Name(a) = &inner.kind else {
                    return;
                };
                if self.env.iter().any(|(p, _)| p == a) {
                    return;
                }
                let Some(Binding::Module(t)) = md.scope.get(a) else {
                    return;
                };
                let Some(b) = ws.modules[*t].own.get(field) else {
                    return;
                };
                let Some((_, end)) = trimmed(&md.text, e.span) else {
                    return;
                };
                if md.text[..end].ends_with(field.as_str()) {
                    let start = end - field.len();
                    let target = Target::Global(b.clone());
                    self.out.push(Occ {
                        start,
                        end,
                        target,
                        decl: false,
                        scope_end: end,
                        site: Some(e.span),
                    });
                }
            }
            ExprKind::App(f, args) => {
                self.expr(f);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Lambda(p, body) => {
                // A lambda's span starts at its parameter.
                let Some((start, scope_end)) = trimmed(&md.text, e.span) else {
                    return;
                };
                let at = if md.text[start..].starts_with(p.as_str()) {
                    start
                } else {
                    usize::MAX
                };
                if at != usize::MAX {
                    let end = start + p.len();
                    self.out.push(Occ {
                        start,
                        end,
                        target: Target::Local(at),
                        decl: true,
                        scope_end,
                        site: None,
                    });
                }
                self.env.push((p.clone(), at));
                self.expr(body);
                self.env.pop();
            }
            ExprKind::Record(fs) => fs.iter().for_each(|(_, v)| self.expr(v)),
            ExprKind::List(xs) => xs.iter().for_each(|x| self.expr(x)),
            ExprKind::Lit(_) | ExprKind::Field(..) | ExprKind::Sql(_) | ExprKind::Error => {}
        }
    }
}

/// Byte range of a span without surrounding whitespace.
fn trimmed(text: &str, span: Span) -> Option<(usize, usize)> {
    let (s, e) = (span.start as usize, span.end as usize);
    let t = text.get(s..e)?;
    let start = s + (t.len() - t.trim_start().len());
    let end = e - (t.len() - t.trim_end().len());
    (start < end).then_some((start, end))
}

/// The range where `name` is written at `span`: the name itself, or an
/// operator symbol `+` for `_+_`.
fn spelled(text: &str, span: Span, name: &str) -> Option<(usize, usize)> {
    let (s, e) = trimmed(text, span)?;
    let t = &text[s..e];
    (t == name || name.strip_prefix('_').and_then(|n| n.strip_suffix('_')) == Some(t))
        .then_some((s, e))
}

/// The occurrence under the cursor; at a boundary (`a|+`), the one ending
/// there if none starts there.
fn occ_at(occs: &[Occ], offset: usize) -> Option<&Occ> {
    occs.iter()
        .find(|o| o.start <= offset && offset < o.end)
        .or_else(|| occs.iter().find(|o| o.end == offset))
}

fn root_range(ws: &Workspace, start: usize, end: usize) -> Range {
    let text = &ws.modules[ws.root].text;
    Range {
        start: position_of(text, start),
        end: position_of(text, end),
    }
}

// ── requests ─────────────────────────────────────────────────

/// Markdown for the name under the cursor. At a use, its type there (the
/// definition's scheme instantiated, as far as the checker got), followed
/// by the definition's general type (every candidate for an overload set)
/// when that differs.
pub fn hover(ws: &Workspace, pos: Position) -> Option<String> {
    let text = &ws.modules[ws.root].text;
    let occs = occurrences(ws);
    let o = occ_at(&occs, offset_at(text, pos)?)?;
    let name = &text[o.start..o.end];
    let tc = check(ws);
    let here = o.site.and_then(|sp| tc.use_type(ws.root, sp));
    let Target::Global(b) = &o.target else {
        let head = here.map_or_else(|| name.to_string(), |t| format!("{name} : {t}"));
        return Some(format!("```cagara\n{head}\n```\nlambda parameter"));
    };
    let (lines, def_name) = match b {
        Binding::Def(m, i) => (
            vec![def_line(&tc, ws, *m, *i)],
            ws.modules[*m].module.defs[*i].name.as_str(),
        ),
        Binding::Overloads(m, is) => {
            let lines = is.iter().map(|&i| def_line(&tc, ws, *m, i)).collect();
            (
                lines,
                is.first()
                    .map_or(name, |&i| ws.modules[*m].module.defs[i].name.as_str()),
            )
        }
        Binding::Module(t) => {
            return Some(format!(
                "```cagara\nmodule {}\n```",
                ws.modules[*t].path.display()
            ))
        }
        Binding::Prim(_) => (vec![format!("{name} : primitive")], name),
    };
    let general = format!("```cagara\n{}\n```", lines.join("\n"));
    match here.map(|t| format!("{def_name} : {t}")) {
        Some(line) if lines != [line.as_str()] => Some(format!(
            "```cagara\n{line}\n```\n---\ndefined as\n{general}"
        )),
        _ => Some(general),
    }
}

/// Where the name under the cursor is defined: file and range of the name
/// in each definition (several for an overload set), or the parameter of a
/// lambda. The prelude has no file, so its definitions are not returned.
pub fn definition(ws: &Workspace, pos: Position) -> Vec<(PathBuf, Range)> {
    let text = &ws.modules[ws.root].text;
    let occs = occurrences(ws);
    let Some(o) = offset_at(text, pos).and_then(|off| occ_at(&occs, off)) else {
        return vec![];
    };
    let (m, is) = match &o.target {
        Target::Local(at) => {
            let root = &ws.modules[ws.root];
            return vec![(
                root.path.clone(),
                root_range(ws, *at, *at + (o.end - o.start)),
            )];
        }
        Target::Global(Binding::Def(m, i)) => (*m, vec![*i]),
        Target::Global(Binding::Overloads(m, is)) => (*m, is.clone()),
        Target::Global(_) => return vec![],
    };
    if m == 0 {
        return vec![];
    }
    let md = &ws.modules[m];
    is.iter()
        .map(|&i| {
            let d = &md.module.defs[i];
            let start = d.span.start as usize;
            let range = Range {
                start: position_of(&md.text, start),
                end: position_of(&md.text, start + d.name.len()),
            };
            (md.path.clone(), range)
        })
        .collect()
}

/// Ranges in the root file that refer to the same thing as the name under
/// the cursor, with or without its binding site.
pub fn references(ws: &Workspace, pos: Position, include_declaration: bool) -> Vec<Range> {
    highlights(ws, pos)
        .into_iter()
        .filter(|(_, k)| include_declaration || *k != DocumentHighlightKind::WRITE)
        .map(|(r, _)| r)
        .collect()
}

/// Occurrences in the root file of the name under the cursor; binding
/// sites are `WRITE`, uses `READ`.
pub fn highlights(ws: &Workspace, pos: Position) -> Vec<(Range, DocumentHighlightKind)> {
    let text = &ws.modules[ws.root].text;
    let occs = occurrences(ws);
    let Some(o) = offset_at(text, pos).and_then(|off| occ_at(&occs, off)) else {
        return vec![];
    };
    occs.iter()
        .filter(|x| x.target == o.target)
        .map(|x| {
            let kind = if x.decl {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            };
            (root_range(ws, x.start, x.end), kind)
        })
        .collect()
}

/// The outline of the root file: imports, then definitions with their
/// inferred types.
#[allow(deprecated)] // `DocumentSymbol::deprecated` must still be set.
pub fn symbols(ws: &Workspace) -> Vec<DocumentSymbol> {
    let tc = check(ws);
    let md = &ws.modules[ws.root];
    let sym = |name: String, detail: Option<String>, kind, range: Range, selection_range: Range| {
        DocumentSymbol {
            name,
            detail,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: None,
        }
    };
    let mut out: Vec<DocumentSymbol> = md
        .module
        .imports
        .iter()
        .filter_map(|imp| {
            let (s, e) = trimmed(&md.text, imp.span)?;
            let r = root_range(ws, s, e);
            Some(sym(
                imp.path.clone(),
                imp.alias.clone(),
                SymbolKind::MODULE,
                r,
                r,
            ))
        })
        .collect();
    for (i, d) in md.module.defs.iter().enumerate() {
        let Some((s, e)) = trimmed(&md.text, d.span) else {
            continue;
        };
        let name_end = (s + d.name.len()).min(e);
        let ty = tc.type_of(ws.root, i).map(str::to_string);
        let kind = if ty.as_deref().is_some_and(|t| t.contains("->")) {
            SymbolKind::FUNCTION
        } else {
            SymbolKind::VARIABLE
        };
        out.push(sym(
            d.name.clone(),
            ty,
            kind,
            root_range(ws, s, e),
            root_range(ws, s, name_end),
        ));
    }
    out
}

/// Names that can be written at the cursor: after `alias.`, the imported
/// module's definitions; after `.`, `.<`, or `.>`, the columns of that row
/// (see `field_completion`); after `expr.`, nothing. Otherwise every name
/// in scope plus the lambda parameters around the cursor. Operators and
/// `__` primitives are left out. The client filters by prefix.
///
/// Takes the workspace mutably: column completion checks a probed copy of
/// the text and then restores it.
pub fn completion(ws: &mut Workspace, pos: Position) -> Vec<CompletionItem> {
    let text = &ws.modules[ws.root].text;
    let Some(offset) = offset_at(text, pos) else {
        return vec![];
    };
    match dot_context(text, offset) {
        Dot::Column(start, end) => field_completion(ws, start, end),
        Dot::Member(alias) => {
            let ws: &Workspace = ws;
            let Some(Binding::Module(t)) = ws.modules[ws.root].scope.get(&alias) else {
                return vec![];
            };
            let tc = check(ws);
            let mut items: Vec<CompletionItem> = ws.modules[*t]
                .own
                .iter()
                .filter(|(n, _)| is_name(n))
                .map(|(n, b)| item(ws, &tc, n, b))
                .collect();
            items.sort_by(|x, y| x.label.cmp(&y.label));
            items
        }
        Dot::None => name_completion(ws, offset),
    }
}

/// What the identifier at the cursor follows.
enum Dot {
    /// `.x`, `.<x`, `.>x`: a column; the byte range of the partial name.
    Column(usize, usize),
    /// `name.x`: a module alias, or else a record projection.
    Member(String),
    None,
}

fn dot_context(text: &str, offset: usize) -> Dot {
    let b = text.as_bytes();
    let mut start = offset;
    while start > 0 && is_ident(b[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < b.len() && is_ident(b[end]) {
        end += 1;
    }
    if start >= 2 && b[start - 2] == b'.' && matches!(b[start - 1], b'<' | b'>') {
        return Dot::Column(start, end);
    }
    if start == 0 || b[start - 1] != b'.' {
        return Dot::None;
    }
    let dot = start - 1;
    let mut a = dot;
    while a > 0 && is_ident(b[a - 1]) {
        a -= 1;
    }
    if a < dot {
        return Dot::Member(text[a..dot].to_string());
    }
    // A dot after a closing bracket or string projects a value.
    if a > 0 && matches!(b[a - 1], b')' | b'}' | b']' | b'"') {
        return Dot::Member(String::new());
    }
    Dot::Column(start, end)
}

/// Columns for the reference at `start..end`: the file is checked with
/// `PROBE_FIELD` written there, which records what its row is known to
/// have (a table's columns, a join side's, or those used next to it), and
/// then the text is restored. Nothing if the row is unknown.
fn field_completion(ws: &mut Workspace, start: usize, end: usize) -> Vec<CompletionItem> {
    let root = ws.root;
    let original = ws.modules[root].text.clone();
    let probed = format!("{}{PROBE_FIELD}{}", &original[..start], &original[end..]);
    let mut fields: Vec<(String, String)> = Vec::new();
    if ws.set_source(root, probed) {
        let tc = check(ws);
        fields = tc.probe_fields(root).map(<[_]>::to_vec).unwrap_or_default();
    }
    let restored = ws.set_source(root, original);
    debug_assert!(restored, "restoring the text keeps its imports");
    fields
        .into_iter()
        .filter(|(n, _)| n != PROBE_FIELD)
        .enumerate()
        .map(|(i, (label, ty))| CompletionItem {
            label,
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(ty),
            // Keep the row's declaration order rather than the alphabet.
            sort_text: Some(format!("{i:04}")),
            ..Default::default()
        })
        .collect()
}

fn name_completion(ws: &Workspace, offset: usize) -> Vec<CompletionItem> {
    let tc = check(ws);
    let md = &ws.modules[ws.root];
    let text = &md.text;
    let mut locals: Vec<CompletionItem> = occurrences(ws)
        .into_iter()
        .filter(|o| {
            o.decl
                && matches!(o.target, Target::Local(_))
                && o.start < offset
                && offset <= o.scope_end
        })
        .map(|o| CompletionItem {
            label: text[o.start..o.end].to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some("lambda parameter".into()),
            ..Default::default()
        })
        .collect();
    locals.sort_by(|x, y| x.label.cmp(&y.label));
    locals.dedup_by(|x, y| x.label == y.label);
    let shadowed: HashSet<String> = locals.iter().map(|i| i.label.clone()).collect();
    let mut items: Vec<CompletionItem> = md
        .scope
        .iter()
        .filter(|(n, b)| is_name(n) && !shadowed.contains(*n) && !matches!(b, Binding::Prim(_)))
        .map(|(n, b)| item(ws, &tc, n, b))
        .collect();
    items.extend(locals);
    items.sort_by(|x, y| x.label.cmp(&y.label));
    items
}

fn item(ws: &Workspace, tc: &TypeCheck, name: &str, b: &Binding) -> CompletionItem {
    let (detail, kind) = match b {
        Binding::Def(m, i) => {
            let ty = tc.type_of(*m, *i).unwrap_or("(type error)").to_string();
            let k = if ty.contains("->") {
                CompletionItemKind::FUNCTION
            } else {
                CompletionItemKind::VARIABLE
            };
            (ty, k)
        }
        Binding::Overloads(m, is) => {
            let first = is
                .first()
                .and_then(|&i| tc.type_of(*m, i))
                .unwrap_or("(type error)");
            let more = if is.len() > 1 {
                format!(" (+{} overloads)", is.len() - 1)
            } else {
                String::new()
            };
            (format!("{first}{more}"), CompletionItemKind::FUNCTION)
        }
        Binding::Module(t) => (
            format!("module {}", ws.modules[*t].path.display()),
            CompletionItemKind::MODULE,
        ),
        Binding::Prim(_) => ("primitive".into(), CompletionItemKind::FUNCTION),
    };
    CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail: Some(detail),
        ..Default::default()
    }
}

/// A definition's hover line, under its own name (`_+_` for `+`).
fn def_line(tc: &TypeCheck, ws: &Workspace, m: usize, i: usize) -> String {
    let name = &ws.modules[m].module.defs[i].name;
    format!("{name} : {}", tc.type_of(m, i).unwrap_or("(type error)"))
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A user-facing identifier: not an operator (`_+_`) or a `__` primitive.
fn is_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.iter().all(|&c| is_ident(c))
        && !b[0].is_ascii_digit()
        && !n.starts_with("__")
}

/// Formatting edits for the root file: one edit replacing the whole text, or
/// none if it is already formatted. Definitions with syntax errors are left
/// as written, so formatting works while a file is being edited.
pub fn format(ws: &Workspace) -> Result<Vec<TextEdit>, cagara_fmt::FmtError> {
    let text = &ws.modules[ws.root].text;
    let out = cagara_fmt::format(text)?.text;
    if &out == text {
        return Ok(vec![]);
    }
    let range = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: position_of(text, text.len()),
    };
    Ok(vec![TextEdit {
        range,
        new_text: out,
    }])
}

/// Byte offset of an LSP position (UTF-16 columns).
pub fn offset_at(text: &str, pos: Position) -> Option<usize> {
    let mut line_start = 0;
    for _ in 0..pos.line {
        line_start += text[line_start..].find('\n')? + 1;
    }
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |i| line_start + i);
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
    Position {
        line: before.matches('\n').count() as u32,
        character: before[line_start..].encode_utf16().count() as u32,
    }
}

/// A diagnostic's range from its line, byte column, and width in chars.
fn diag_range(d: &Diag) -> Range {
    let line = d.line.saturating_sub(1) as u32;
    let col = d.col.saturating_sub(1);
    let start = d
        .source
        .get(..col)
        .map_or(col, |s| s.encode_utf16().count()) as u32;
    let width = d.source.get(col..).map_or(1, |s| {
        s.chars()
            .take(d.width.max(1))
            .map(char::len_utf16)
            .sum::<usize>()
            .max(1)
    }) as u32;
    Range {
        start: Position {
            line,
            character: start,
        },
        end: Position {
            line,
            character: start + width,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SRC: &str =
        "users : query { id = int, name = string, age = int } = table \"p\" \"users\"\n\
                       adult = .age >= 18\n\
                       q = users & where adult & agg { s = sum .age }\n\
                       bad = users & select { x = \"é\" <> .age }\n\
                       twice = x => x + x\n\
                       shadow = users => users & where adult\n";

    fn ws() -> Workspace {
        Workspace::open_with(Path::new("/nonexistent/main.cagara"), SRC.to_string())
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range {
            start: pos(line, start),
            end: pos(line, end),
        }
    }

    #[test]
    fn formatting_replaces_the_whole_document() {
        let edits = format(&ws()).unwrap();
        let [edit] = edits.as_slice() else {
            panic!("{edits:?}")
        };
        assert_eq!(
            edit.range,
            Range {
                start: pos(0, 0),
                end: pos(6, 0)
            }
        );
        assert!(edit
            .new_text
            .contains("q = users\n  & where adult\n  & agg { s = sum .age }\n"));
        let formatted =
            Workspace::open_with(Path::new("/nonexistent/main.cagara"), edit.new_text.clone());
        assert!(format(&formatted).unwrap().is_empty());
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
        let (here, general) = h.split_once("defined as").unwrap();
        assert!(
            here.contains("sum : expr { age = int, id = int, name = string } int -> "),
            "{h}"
        );
        assert_eq!(general.matches("sum : ").count(), 4, "{h}");
        // Operators hover at their symbol, under their definition name.
        let h = hover(&ws, pos(4, 15)).unwrap();
        assert!(h.contains("_+_ : "), "{h}");
        // A column reference has no binding.
        assert!(hover(&ws, pos(1, 10)).is_none());
        // A lambda parameter shadows the table.
        let h = hover(&ws, pos(5, 19)).unwrap();
        assert!(h.contains("lambda parameter"), "{h}");
    }

    #[test]
    fn hover_shows_the_type_at_the_use() {
        let src = "users : query { id = int, name = string } = table \"p\" \"users\"\n\
                   q = users & select { n = .name }\n\
                   twice = x => x + x\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        // `select` at its use: instantiated, then the general scheme.
        let h = hover(&ws, pos(1, 14)).unwrap();
        let (here, general) = h.split_once("defined as").unwrap();
        assert!(
            here.contains("select : { n = expr { name = string, id = int } string } -> "),
            "{h}"
        );
        assert!(here.contains("-> query { n = string }"), "{h}");
        assert!(general.contains("select : a -> query b -> query c"), "{h}");
        // A monomorphic use shows its type once.
        let h = hover(&ws, pos(1, 5)).unwrap();
        assert!(
            !h.contains("defined as") && h.contains("users : query { id = int, name = string }"),
            "{h}"
        );
        // A lambda parameter shows its inferred type.
        let h = hover(&ws, pos(2, 13)).unwrap();
        assert!(
            h.contains("x : expr ") && h.contains("lambda parameter"),
            "{h}"
        );
    }

    #[test]
    fn definition_points_at_the_name() {
        let ws = ws();
        let d = definition(&ws, pos(2, 19));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, range(1, 0, 5));
        // Prelude definitions have no file.
        assert!(definition(&ws, pos(2, 37)).is_empty());
        // A parameter use jumps to the parameter.
        let d = definition(&ws, pos(5, 19));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, range(5, 9, 14));
    }

    #[test]
    fn references_respect_shadowing() {
        let ws = ws();
        // `users` from its use in `q`: the definition and two uses, not the
        // lambda parameter in `shadow`.
        let rs = references(&ws, pos(2, 5), true);
        assert_eq!(rs, vec![range(0, 0, 5), range(2, 4, 9), range(3, 6, 11)]);
        let rs = references(&ws, pos(2, 5), false);
        assert_eq!(rs, vec![range(2, 4, 9), range(3, 6, 11)]);
        // `x` in `twice`: the parameter and both uses.
        let hs = highlights(&ws, pos(4, 13));
        let kinds: Vec<_> = hs.iter().map(|(_, k)| *k).collect();
        assert_eq!(
            hs.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![range(4, 8, 9), range(4, 13, 14), range(4, 17, 18)]
        );
        assert_eq!(
            kinds,
            vec![
                DocumentHighlightKind::WRITE,
                DocumentHighlightKind::READ,
                DocumentHighlightKind::READ
            ]
        );
    }

    #[test]
    fn symbols_list_definitions() {
        let ss = symbols(&ws());
        let names: Vec<&str> = ss.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["users", "adult", "q", "bad", "twice", "shadow"]);
        assert_eq!(ss[1].selection_range, range(1, 0, 5));
        assert_eq!(ss[1].range, range(1, 0, 18));
        assert_eq!(ss[1].kind, SymbolKind::VARIABLE);
        assert_eq!(ss[4].kind, SymbolKind::FUNCTION);
        assert!(ss[4].detail.as_deref().is_some_and(|d| d.contains("->")));
    }

    fn fields(src: &str, p: Position) -> Vec<String> {
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let items = completion(&mut ws, p);
        assert!(
            items
                .iter()
                .all(|i| i.kind == Some(CompletionItemKind::FIELD)),
            "{items:?}"
        );
        // The probe leaves no trace.
        assert_eq!(ws.modules[ws.root].text, src);
        items.into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn column_completion_uses_the_row() {
        let src = "users : query { id = int, name = string, age = int } = table \"p\" \"users\"\n\
                   orders : query { oid = int, user_id = int } = table \"p\" \"orders\"\n\
                   a = users & where (.ag >= 18)\n\
                   b = users & orders ? .<id == .>u\n\
                   c = .age >= 18 && .\n";
        // A partial name inside a stage: the table's columns, in order.
        assert_eq!(fields(src, pos(2, 22)), ["id", "name", "age"]);
        // Join sides.
        assert_eq!(fields(src, pos(3, 24)), ["id", "name", "age"]);
        assert_eq!(fields(src, pos(3, 32)), ["oid", "user_id"]);
        // An open predicate knows the columns used next to it.
        assert_eq!(fields(src, pos(4, 22)), ["age"]);
    }

    #[test]
    fn column_completion_while_typing() {
        // The line does not parse yet: the probe still sees the table.
        let src = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
                   c = users & select { n = .\n";
        assert_eq!(fields(src, pos(1, 26)), ["id", "age"]);
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let before = diagnostics(&ws);
        completion(&mut ws, pos(1, 26));
        assert_eq!(diagnostics(&ws), before);
    }

    #[test]
    fn completion_lists_scope_and_parameters() {
        let mut ws = ws();
        let mut labels = |p| {
            completion(&mut ws, p)
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        // Inside `twice`'s body: its parameter, top-level names, the prelude.
        let ls = labels(pos(4, 13));
        for n in ["x", "adult", "users", "sum", "where"] {
            assert!(ls.iter().any(|l| l == n), "{n} missing: {ls:?}");
        }
        assert!(!ls.iter().any(|l| l.starts_with('_')), "{ls:?}");
        // Outside the lambda, `x` is not in scope.
        assert!(!labels(pos(2, 4)).iter().any(|l| l == "x"));
        // After a column dot, names are not offered: `adult` alone says
        // nothing about its row but the column being replaced.
        assert!(labels(pos(1, 9)).is_empty());
    }

    #[test]
    fn projection_of_a_value_offers_nothing() {
        let src = "r = { a = 1 }\ns = r.\nt = (r).\n";
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        assert!(completion(&mut ws, pos(1, 6)).is_empty());
        assert!(completion(&mut ws, pos(2, 8)).is_empty());
    }

    #[test]
    fn alias_members_resolve_and_complete() {
        let dir = std::env::temp_dir().join(format!("cagara-lsp-alias-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.cagara"), "helper = .age >= 21\n").unwrap();
        let main = dir.join("main.cagara");
        let src = "import \"lib.cagara\" as lib\nq = lib.helper\nr = lib.\n";
        let mut ws = Workspace::open_with(&main, src.to_string());
        let h = hover(&ws, pos(1, 10)).unwrap();
        assert!(h.contains("helper : "), "{h}");
        let d = definition(&ws, pos(1, 10));
        assert_eq!(d.len(), 1);
        assert!(d[0].0.ends_with("lib.cagara"), "{d:?}");
        assert_eq!(d[0].1, range(0, 0, 6));
        let ls: Vec<String> = completion(&mut ws, pos(2, 8))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(ls, ["helper"]);
        let ss = symbols(&ws);
        assert_eq!(ss[0].kind, SymbolKind::MODULE);
        std::fs::remove_dir_all(&dir).unwrap();
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
