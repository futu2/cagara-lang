//! Diagnostics, hover, definitions, references, highlights, symbols,
//! completion, and formatting for the root file of a workspace. Names are resolved on the
//! AST, so a lambda parameter shadows a top-level name of the same spelling.
//! LSP positions count UTF-16 code units; the workspace uses byte offsets.

use cagara_hir::check::{check_reachable, TypeCheck, PROBE_FIELD};
use cagara_hir::compile_diagnostics;
use cagara_hir::workspace::{Binding, Diag, Workspace};
use cagara_syntax::ast::{Expr, ExprKind, Side, Span};
use cagara_syntax::docs::Docs;
use line_index::{LineIndex, TextRange, TextSize, WideEncoding};
use lsp_types::{
    CompletionItem, CompletionItemKind, DocumentHighlightKind, DocumentSymbol, Position, Range,
    SymbolKind, TextEdit,
};
use std::collections::HashSet;
use std::path::PathBuf;

/// Every diagnostic in the root file: loading (syntax, imports, overloads),
/// type errors, and compilation errors.
pub fn diagnostics(ws: &Workspace) -> Vec<(Range, String)> {
    let compilation_diagnostics = compile_diagnostics(ws);
    let root = ws.modules[ws.root].path.display().to_string();
    // Deduped through a set, not `Vec::contains`: a file being typed has as
    // many diagnostics as it has errors, and comparing each against every one
    // already collected made publishing quadratic in them. The compiler's own
    // collector dedupes the same way (`compile::push_unique`).
    let mut out: Vec<(Range, String)> = Vec::new();
    let mut seen: HashSet<(Range, String)> = HashSet::new();
    for d in compilation_diagnostics.iter().filter(|d| d.path == root) {
        let item = (diag_range(d), d.message.clone());
        if seen.insert(item.clone()) {
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
    ///
    /// The offset is `None` when the parameter is not spelled at the start of
    /// its lambda's span: such a parameter still shadows outer names, but it has
    /// no location to navigate to. A sentinel offset here used to reach
    /// `definition`, where it overflowed (`usize::MAX + len`) in debug builds.
    env: Vec<(String, Option<usize>)>,
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
                    // A use of an unspelled parameter has no site to point at,
                    // so it is not recorded as an occurrence at all.
                    Some(&(_, None)) => return,
                    Some(&(_, Some(at))) => Target::Local(at),
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
                // The parameter carries its own span, so `(x : string) => ...`
                // is as navigable as the bare `x => ...`: the name sits at the
                // start of that span whether or not brackets and a type come
                // before the arrow. Only the name is the occurrence; the
                // annotation is types, not a reference to a name in scope.
                let Some((_, scope_end)) = trimmed(&md.text, e.span) else {
                    return;
                };
                let start = p.span.start as usize;
                let end = start + p.name.len();
                let at = md
                    .text
                    .get(start..end)
                    .filter(|t| *t == p.name)
                    .map(|_| start);
                if let Some(at) = at {
                    self.out.push(Occ {
                        start,
                        end,
                        target: Target::Local(at),
                        decl: true,
                        scope_end,
                        site: None,
                    });
                }
                self.env.push((p.name.clone(), at));
                self.expr(body);
                self.env.pop();
            }
            ExprKind::Record(fs) => fs.iter().for_each(|(_, v)| self.expr(v)),
            ExprKind::List(xs) => xs.iter().for_each(|x| self.expr(x)),
            // Both halves of every arm are names in scope: a condition and the
            // value it selects. A local bound inside an arm's value (`x => ...`)
            // is pushed and popped by the `Lambda` arm above, so arms need no
            // scope handling of their own.
            ExprKind::Case(arms) => arms.iter().for_each(|(cond, value)| {
                self.expr(cond);
                self.expr(value);
            }),
            ExprKind::Lit(_)
            | ExprKind::Field(..)
            | ExprKind::Sql(_)
            | ExprKind::Primitive(_)
            | ExprKind::Error => {}
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

fn root_range(p: &Positions, start: usize, end: usize) -> Range {
    Range {
        start: p.position(start),
        end: p.position(end),
    }
}

/// Hover for a record field where it is *declared*: `age` in
/// `users : query { age = int }`.
///
/// A field is not an item, so it has no type of its own to show — the
/// declaration line beside the cursor already says that. What it does have is
/// the `##` block above it, which is the column's documentation, and asking for
/// it while editing the table is the point of writing it down.
fn declared_field_hover(ws: &Workspace, offset: usize) -> Option<String> {
    let md = &ws.modules[ws.root];
    let docs = Docs::new(&md.text);
    for def in &md.module.defs {
        let Some(ty) = def.ty.as_ref() else {
            continue;
        };
        for record in docs.record_fields(ty) {
            let Some(field) = record.iter().find(|f| contains(f.span, offset)) else {
                continue;
            };
            if let Some(doc) = &field.doc {
                return Some(doc.clone());
            }
        }
    }
    None
}

// ── column provenance ───────────────────────────────────────

/// The documentation of the column at `offset`, from the declaration that named
/// it.
///
/// A column is not a definition. `.age` is a field of a row, and a row is a
/// structural type, so the checker has nothing to point at: unification matches
/// shapes, not declarations, and by the time a use is typed the name of the
/// table it came from is gone. What survives is the *program's* shape — the row
/// a column is read from is the input row of the stage it sits in, and that
/// input is the query the stage is piped from — so this walks that chain down
/// through the steps that keep their columns and stops at the expression whose
/// row a signature fixed. That definition's declared record type is where the
/// column's `##` block is.
///
/// Nothing is shown when the walk reaches no declaration: a column a `select`
/// computed, one that arrived through a join, or one an `agg` named has no
/// single place that named it. Guessing from another table's identically named
/// column would put the wrong words on the screen, which is worse than the type
/// alone.
fn column_doc(
    ws: &Workspace,
    occs: &[Occ],
    offset: usize,
    side: Side,
    field: &str,
) -> Option<String> {
    let md = &ws.modules[ws.root];
    let def = md
        .module
        .defs
        .iter()
        .position(|d| contains(d.span, offset))?;
    let owner = (ws.root, def);
    let body = &md.module.defs.get(def)?.body;
    let descent = Descent {
        ws,
        occs,
        offset,
        side,
    };
    let At::Here(Some((m, i))) = descent.at(ws.root, body, None, owner) else {
        return None;
    };
    let decl = ws.modules.get(m)?;
    let ty = decl.module.defs.get(i)?.ty.as_ref()?;
    Docs::new(&decl.text)
        .record_fields(ty)
        .into_iter()
        .flatten()
        .find(|f| f.name == field)?
        .doc
}

/// A walk from a cursor back to the declaration that named the column it reads.
///
/// One descent per hover, so what it needs that does not change — the workspace,
/// the root file's occurrences, the cursor, and which side of a join the column
/// is on — lives here rather than being threaded through every call.
struct Descent<'a> {
    ws: &'a Workspace,
    occs: &'a [Occ],
    offset: usize,
    side: Side,
}

/// What the descent found at the cursor.
enum At {
    /// The cursor is in this expression; this is the row in effect there.
    Here(Option<(usize, usize)>),
    /// The cursor is in some other expression.
    Elsewhere,
}

impl Descent<'_> {
    /// Descend to the cursor, carrying the definition whose declared record type
    /// fixed the row the sub-expression reads.
    ///
    /// `module` is the module `e` was parsed from: the root while the walk is
    /// in the file being edited, and the defining module while it follows a
    /// body there.
    fn at(
        &self,
        module: usize,
        e: &Expr,
        input: Option<(usize, usize)>,
        owner: (usize, usize),
    ) -> At {
        if !contains(e.span, self.offset) {
            return At::Elsewhere;
        }
        let Some((callee, args)) = spine(e) else {
            return At::Here(input);
        };
        let name = head_name(callee);
        // `q & st`, and the shorthands that carry the stage inline (`q &? pred`,
        // `q &= fields`, `q &| r`): the piped query decides the row, and every
        // later argument is an expression over that row.
        if is_pipeline(name) {
            let Some(q) = args.first() else {
                return At::Here(input);
            };
            let row = self.source(module, q, owner);
            for a in args.iter().skip(1) {
                if contains(a.span, self.offset) {
                    return self.at(module, a, row, owner);
                }
            }
            return self.at(module, q, input, owner);
        }
        // A join's predicate reads the joined row: `.>x` is the query the join
        // names, `.<x` the query piped into it.
        if is_join(name) {
            let row = if self.side == Side::Right {
                args.first().and_then(|q| self.source(module, q, owner))
            } else {
                input
            };
            return args
                .iter()
                .skip(1)
                .find(|a| contains(a.span, self.offset))
                .map_or(At::Elsewhere, |a| self.at(module, a, row, owner));
        }
        // Everything else — a stage applied as a function value (`where p`,
        // `select {..}`), a record, a list, a lambda body — is an expression
        // over whatever row the caller established.
        args.iter()
            .find(|a| contains(a.span, self.offset))
            .map_or(At::Here(input), |a| self.at(module, a, input, owner))
    }

    /// The definition whose declared record type fixed the row of `e`, if one
    /// did.
    ///
    /// A name that declares a record type is the answer. A name that does not —
    /// an alias with no signature (`t = aam.tbl`), or one narrowed to an opaque
    /// `query r` — is a *step*, not a declaration: it says nothing about the
    /// columns, so the walk follows its body instead. The depth bound is what
    /// keeps a file with a cycle (`a = b`, `b = a`, which the checker rejects)
    /// from spinning here.
    fn source(&self, module: usize, e: &Expr, owner: (usize, usize)) -> Option<(usize, usize)> {
        self.source_at(module, e, owner, 8)
    }

    fn source_at(
        &self,
        module: usize,
        e: &Expr,
        owner: (usize, usize),
        depth: u8,
    ) -> Option<(usize, usize)> {
        if depth == 0 {
            return None;
        }
        match &e.kind {
            // A name: its own signature names its columns, if it declares a
            // record type. Otherwise keep following what it stands for.
            ExprKind::Name(_) | ExprKind::Proj(..) => {
                let (m, i) = def_of(self.ws, self.occs, module, e)?;
                if declares_record(self.ws, m, i) {
                    return Some((m, i));
                }
                let body = &self.ws.modules.get(m)?.module.defs.get(i)?.body;
                self.source_at(m, body, (m, i), depth - 1)
            }
            ExprKind::App(..) => {
                let (callee, args) = spine(e)?;
                match head_name(callee) {
                    // `q & st`: the row is `q`'s unless `st` rebuilds the
                    // columns.
                    Some("_&_") => {
                        let q = args.first()?;
                        let keeps = keeps_columns(args.get(1)?);
                        keeps.then(|| self.source_at(module, q, owner, depth))?
                    }
                    // The shorthands, and the set operations that keep the left
                    // row: everything `&…` that does not rebuild the columns.
                    Some("_&?_" | "_&._" | "_&-_" | "_&|_" | "_&!_" | "_&^_" | "_&~_") => {
                        self.source_at(module, args.first()?, owner, depth)
                    }
                    // `union a b` and friends: the left query's row, unchanged.
                    Some("union" | "unionAll" | "intersect" | "except") => {
                        self.source_at(module, args.first()?, owner, depth)
                    }
                    // A table's row is fixed by the signature of the definition
                    // that writes it.
                    Some("table") => Some(owner),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// The `(module, definition)` a name expression denotes.
///
/// The root file's occurrences already know which names a lambda parameter
/// shadows; another module's body is resolved against that module's own scope,
/// which is all a definition's top level needs.
fn def_of(ws: &Workspace, occs: &[Occ], module: usize, e: &Expr) -> Option<(usize, usize)> {
    let binding = if module == ws.root {
        binding_of(occs, e)?.clone()
    } else {
        let md = ws.modules.get(module)?;
        match &e.kind {
            ExprKind::Name(n) => md.scope.get(n)?.clone(),
            ExprKind::Proj(inner, field) => {
                let ExprKind::Name(alias) = &inner.kind else {
                    return None;
                };
                let Binding::Module(t) = md.scope.get(alias)? else {
                    return None;
                };
                ws.modules.get(*t)?.own.get(field)?.clone()
            }
            _ => return None,
        }
    };
    let Binding::Def(m, i) = binding else {
        return None;
    };
    Some((m, i))
}

/// Whether a definition's own signature declares a record type — the only place
/// a column can be documented.
fn declares_record(ws: &Workspace, m: usize, i: usize) -> bool {
    let Some(md) = ws.modules.get(m) else {
        return false;
    };
    let Some(ty) = md.module.defs.get(i).and_then(|d| d.ty.as_ref()) else {
        return false;
    };
    !Docs::new(&md.text).record_fields(ty).is_empty()
}

/// Whether an expression is a pipeline step: `&`, or one of its shorthand
/// spellings, all of which take the query first.
fn is_pipeline(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            "_&_"
                | "_&?_"
                | "_&=_"
                | "_&+_"
                | "_&*_"
                | "_&._"
                | "_&-_"
                | "_&|_"
                | "_&!_"
                | "_&^_"
                | "_&~_"
        )
    )
}

/// Whether a stage expression (`where p`, `order keys`, …) leaves every column
/// of its input row in place.
///
/// `select`, `update`, `agg`, `omit`, `prefix`, `suffix` and `mapValue` build a
/// new row, and the joins and set operations combine two: after any of those,
/// the columns at the cursor are no longer the ones a declaration named.
fn keeps_columns(st: &Expr) -> bool {
    let Some((callee, _)) = spine(st) else {
        return false;
    };
    matches!(
        head_name(callee),
        Some(
            "where"
                | "whereTrue"
                | "order"
                | "limit"
                | "offset"
                | "distinct"
                | "union"
                | "unionAll"
                | "intersect"
                | "except"
        )
    )
}

/// The join spellings: two queries in, a merged row for the predicate.
fn is_join(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            "innerJoin"
                | "leftJoin"
                | "rightJoin"
                | "fullJoin"
                | "semiJoin"
                | "antiJoin"
                | "_?_"
                | "_<?_"
                | "_?>_"
                | "_<?>_"
        )
    )
}

/// An application's callee and arguments, flattening the curried nesting
/// (`innerJoin q pred` is `App(App(innerJoin, [q]), [pred])`).
fn spine(e: &Expr) -> Option<(&Expr, Vec<&Expr>)> {
    let ExprKind::App(head, args) = &e.kind else {
        return None;
    };
    let mut callee = head.as_ref();
    let mut args: Vec<&Expr> = args.iter().collect();
    while let ExprKind::App(inner, more) = &callee.kind {
        let mut all: Vec<&Expr> = more.iter().collect();
        all.append(&mut args);
        args = all;
        callee = inner.as_ref();
    }
    Some((callee, args))
}

/// The name an expression applies, when it is spelled as one.
fn head_name(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Name(n) => Some(n),
        _ => None,
    }
}

/// The binding a name expression resolved to, from the occurrence list — which
/// already knows which names a lambda parameter shadows.
fn binding_of<'a>(occs: &'a [Occ], e: &Expr) -> Option<&'a Binding> {
    let o = occs.iter().find(|o| o.site == Some(e.span))?;
    match &o.target {
        Target::Global(b) => Some(b),
        Target::Local(_) => None,
    }
}

fn contains(span: Span, offset: usize) -> bool {
    let (start, end) = (span.start as usize, span.end as usize);
    start <= offset && offset < end
}

// ── requests ─────────────────────────────────────────────────

/// Markdown for the name under the cursor. At a use, its type there (the
/// definition's scheme instantiated, as far as the checker got), followed
/// by the definition's general type (every candidate for an overload set)
/// when that differs, and by the definition's own `##` documentation.
///
/// The doc text is the same text `cagara doc` prints — one source for both, so
/// a reader in the editor and a reader of the reference see the same words.
pub fn hover(ws: &Workspace, pos: Position) -> Option<String> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let offset = p.offset(pos)?;
    let occs = occurrences(ws);
    let Some(o) = occ_at(&occs, offset) else {
        return declared_field_hover(ws, offset).or_else(|| atom_hover(ws, &occs, offset));
    };
    let name = &text[o.start..o.end];
    let tc = check_reachable(ws);
    let here = o.site.and_then(|sp| tc.use_type(ws.root, sp));
    let Target::Global(b) = &o.target else {
        let head = here.map_or_else(|| name.to_string(), |t| sig(name, t));
        return Some(format!("```cagara\n{head}\n```\nlocal binding"));
    };
    // (name, type, docs) of the definition, or of every candidate.
    let def_ty = |m: usize, i: usize| {
        let md = &ws.modules[m];
        let d = &md.module.defs[i];
        (
            d.name.as_str(),
            tc.type_of(m, i).unwrap_or("(type error)"),
            cagara_syntax::docs::item_doc(&md.text, d.span.start as usize),
        )
    };
    let defs: Vec<(&str, &str, Option<String>)> = match b {
        Binding::Def(m, i) => vec![def_ty(*m, *i)],
        Binding::Overloads(m, is) => is.iter().map(|&i| def_ty(*m, i)).collect(),
        Binding::Module(t) => {
            return Some(format!(
                "```cagara\nmodule {}\n```",
                ws.modules[*t].path.display()
            ))
        }
    };
    let general = defs
        .iter()
        .map(|(n, t, _)| sig(n, t))
        .collect::<Vec<_>>()
        .join("\n");
    let general = format!("```cagara\n{general}\n```");
    let mut md = match (here, defs.first()) {
        (Some(t), Some((n0, t0, _))) if !(defs.len() == 1 && *t0 == t) => {
            format!("```cagara\n{}\n```\n---\ndefined as\n{general}", sig(n0, t))
        }
        _ => general,
    };
    // One overload documented and another not is still the best text there is
    // for the name: the first non-empty doc describes what every candidate
    // does, and the signatures above already say how they differ.
    if let Some(doc) = defs.iter().find_map(|(_, _, d)| d.as_deref()) {
        md.push_str("\n\n---\n\n");
        md.push_str(doc);
    }
    Some(md)
}

/// Width hover signatures are laid out in; popups are narrower than files.
const HOVER_WIDTH: usize = 72;

/// `name : ty`, broken over lines like a signature when it is long.
fn sig(name: &str, ty: &str) -> String {
    cagara_fmt::format_type(name, ty, HOVER_WIDTH)
}

/// What the atom under the cursor is, which decides how its type is spelled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Atom {
    /// A column reference, whose type is an expression over the row.
    Column(Side),
    /// A literal, whose recorded type is the value type it was lifted to.
    Literal,
}

/// Hover for a column reference (`.age`, `.<id`) or a literal under the cursor.
///
/// A column is spelled as the expression type it has — `expr r string`, or
/// `expr (join r s) int` for a side-qualified one — because a column is not a
/// bare value: it is row-phase, it is what every `expr r a` parameter accepts,
/// and the bare value type hides both. `r` is the input row of the enclosing
/// stage, the same name a signature gives it; printing the concrete row instead
/// would repeat the whole table for every column and put the column first,
/// which is not the row's order.
///
/// A literal keeps the type it was lifted to (`18` against a `float` column is
/// `float`) and is not wrapped: a literal may be a *label* rather than an
/// expression (`limit 2`, `prefix "u_"`), where `expr r …` would be wrong.
///
/// A column also carries its own documentation, when [`column_doc`] can follow
/// the row back to the declaration that named it: the type says what the column
/// *is* at this use, and the `##` block says what the column *means*.
fn atom_hover(ws: &Workspace, occs: &[Occ], offset: usize) -> Option<String> {
    let md = &ws.modules[ws.root];
    let mut found = None;
    for d in &md.module.defs {
        atom_at(&md.text, &d.body, offset, &mut found);
    }
    let (span, (start, end), atom) = found?;
    let ty = check_reachable(ws).use_type(ws.root, span)?.to_string();
    let shown = match atom {
        Atom::Column(side) => format!("expr {} {}", row_of(side), value_arg(&ty)),
        Atom::Literal => ty,
    };
    let mut out = format!("```cagara\n{}\n```", sig(md.text.get(start..end)?, &shown));
    if let Atom::Column(side) = atom {
        // `.age` / `.<id` / `.>id`: the name is the token after the dots.
        let token = md.text.get(start..end)?;
        let field = token.trim_start_matches('.').trim_start_matches(['<', '>']);
        if let Some(doc) = column_doc(ws, occs, offset, side, field) {
            out.push_str("\n\n---\n\n");
            out.push_str(&doc);
        }
    }
    Some(out)
}

/// The row a column reference is an expression over: the input row, or the
/// joined row for the side-qualified form.
fn row_of(side: Side) -> &'static str {
    match side {
        Side::Single => "r",
        Side::Left | Side::Right => "(join r s)",
    }
}

/// `expr`'s value argument, parenthesised when it is compound — the same
/// parenthesisation the checker's printer applies, so `expr r (maybe float)`
/// parses as the type it means.
fn value_arg(ty: &str) -> String {
    if ty.contains(' ') {
        format!("({ty})")
    } else {
        ty.to_string()
    }
}

/// The column reference or literal whose trimmed range contains `offset`
/// (or ends at it, as for names).
fn atom_at(text: &str, e: &Expr, offset: usize, found: &mut Option<(Span, (usize, usize), Atom)>) {
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Field(..) => {
            if let Some((s, t)) = trimmed(text, e.span) {
                let inside = s <= offset && offset < t;
                if inside || (offset == t && found.is_none()) {
                    let atom = match &e.kind {
                        ExprKind::Field(side, _) => Atom::Column(*side),
                        _ => Atom::Literal,
                    };
                    *found = Some((e.span, (s, t), atom));
                }
            }
        }
        ExprKind::Proj(inner, _) => atom_at(text, inner, offset, found),
        ExprKind::App(f, args) => {
            atom_at(text, f, offset, found);
            args.iter().for_each(|a| atom_at(text, a, offset, found));
        }
        ExprKind::Lambda(_, body) => atom_at(text, body, offset, found),
        ExprKind::Record(fs) => fs.iter().for_each(|(_, v)| atom_at(text, v, offset, found)),
        ExprKind::List(xs) => xs.iter().for_each(|x| atom_at(text, x, offset, found)),
        // Hover works on a column or literal anywhere, arms included.
        ExprKind::Case(arms) => arms.iter().for_each(|(cond, value)| {
            atom_at(text, cond, offset, found);
            atom_at(text, value, offset, found);
        }),
        ExprKind::Name(_) | ExprKind::Sql(_) | ExprKind::Primitive(_) | ExprKind::Error => {}
    }
}

/// Where the name under the cursor is defined: file and range of the name
/// in each definition (several for an overload set), or the parameter of a
/// lambda. The prelude has no file, so its definitions are not returned.
pub fn definition(ws: &Workspace, pos: Position) -> Vec<(PathBuf, Range)> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let occs = occurrences(ws);
    let Some(o) = p.offset(pos).and_then(|off| occ_at(&occs, off)) else {
        return vec![];
    };
    let (m, is) = match &o.target {
        Target::Local(at) => {
            let root = &ws.modules[ws.root];
            return vec![(
                root.path.clone(),
                root_range(&p, *at, *at + (o.end - o.start)),
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
    let mp = Positions::new(&md.text);
    is.iter()
        .map(|&i| {
            let d = &md.module.defs[i];
            let start = d.span.start as usize;
            let range = Range {
                start: mp.position(start),
                end: mp.position(start + d.name.len()),
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
    let p = Positions::new(text);
    let occs = occurrences(ws);
    let Some(o) = p.offset(pos).and_then(|off| occ_at(&occs, off)) else {
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
            (root_range(&p, x.start, x.end), kind)
        })
        .collect()
}

/// The outline of the root file: imports, then definitions with their
/// inferred types.
#[allow(deprecated)] // `DocumentSymbol::deprecated` must still be set.
pub fn symbols(ws: &Workspace) -> Vec<DocumentSymbol> {
    let tc = check_reachable(ws);
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
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
            let r = root_range(&p, s, e);
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
            root_range(&p, s, e),
            root_range(&p, s, name_end),
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
/// Column completion checks a speculative workspace snapshot, leaving the
/// shared live module graph untouched.
pub fn completion(ws: &mut Workspace, pos: Position) -> Vec<CompletionItem> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let Some(offset) = p.offset(pos) else {
        return vec![];
    };
    match dot_context(text, offset) {
        Dot::Column(start, end) => field_completion(ws, start, end),
        Dot::Member(alias) => {
            let ws: &Workspace = &*ws;
            let Some(Binding::Module(t)) = ws.modules[ws.root].scope.get(&alias) else {
                return vec![];
            };
            let tc = check_reachable(ws);
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
        // `(...)`.x` projects a value's field: nothing to complete.
        Dot::Value => vec![],
    }
}

/// What the identifier at the cursor follows.
enum Dot {
    /// `.x`, `.<x`, `.>x`: a column; the byte range of the partial name.
    Column(usize, usize),
    /// `name.x`: a module alias, or else a record projection.
    Member(String),
    /// A dot after a closing bracket or string: a projection of a value that
    /// has no named members to complete.
    Value,
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
        return Dot::Value;
    }
    Dot::Column(start, end)
}

/// Columns for the reference at `start..end`: the file is checked with
/// `PROBE_FIELD` written there, which records what its row is known to
/// have (a table's columns, a join side's, or those used next to it).
/// Nothing if the row is unknown. The probe is a throwaway module in the live
/// database, so it does not rebuild this graph the way `snapshot()` did.
fn field_completion(ws: &mut Workspace, start: usize, end: usize) -> Vec<CompletionItem> {
    let original = &ws.modules[ws.root].text;
    let probed = format!("{}{PROBE_FIELD}{}", &original[..start], &original[end..]);
    let fields: Vec<(String, String)> = ws.probe_fields(&probed).unwrap_or_default();
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
    let tc = check_reachable(ws);
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
            detail: Some("local binding".into()),
            ..Default::default()
        })
        .collect();
    locals.sort_by(|x, y| x.label.cmp(&y.label));
    locals.dedup_by(|x, y| x.label == y.label);
    let shadowed: HashSet<String> = locals.iter().map(|i| i.label.clone()).collect();
    let mut items: Vec<CompletionItem> = md
        .scope
        .iter()
        .filter(|(n, _)| is_name(n) && !shadowed.contains(*n))
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
    };
    CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail: Some(detail),
        ..Default::default()
    }
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

/// Formatting edits for the root file: the lines that change, or none if it is
/// already formatted. Definitions with syntax errors are left as written, so
/// formatting works while a file is being edited.
///
/// The edits are per changed line ([`crate::edits::between`]), not one range
/// over the whole file. A whole-file range is the most expensive thing an
/// editor can be asked to apply: every token, fold and undo entry goes, the
/// cursor and selection move, and a client that mirrors the buffer elsewhere
/// hands the file to whatever mirrors it. Formatting one definition used to
/// cost the editor exactly what reformatting the file by hand costs.
pub fn format(ws: &Workspace) -> Result<Vec<TextEdit>, cagara_fmt::FmtError> {
    let text = &ws.modules[ws.root].text;
    let out = cagara_fmt::format(text)?.text;
    if &out == text {
        return Ok(vec![]);
    }
    Ok(crate::edits::between(text, &out))
}

/// Byte↔position mapping for one text. Built once per request: line-index
/// scans the text once and binary-searches after, where per-call rescans
/// made hover and highlights O(lines × occurrences) on large files.
struct Positions<'a> {
    text: &'a str,
    idx: LineIndex,
}

impl<'a> Positions<'a> {
    fn new(text: &'a str) -> Self {
        Positions {
            text,
            idx: LineIndex::new(text),
        }
    }

    /// Byte offset of an LSP position (UTF-16 columns), clamped to the
    /// line's end; None for a line the text does not have.
    fn offset(&self, pos: Position) -> Option<usize> {
        let line = self.idx.line(pos.line)?;
        let end = self.line_end(line);
        let wide = line_index::WideLineCol {
            line: pos.line,
            col: pos.character,
        };
        let Some(lc) = self.idx.to_utf8(WideEncoding::Utf16, wide) else {
            // A column so large the wide-character walk overflows: the line
            // end, where a per-unit walk would also stop.
            return Some(end);
        };
        // The byte offset is the line's start plus the UTF-8 column, added in
        // `usize`. `LineIndex::offset` does the same sum in `u32` — the type
        // LSP columns and `TextSize` are both made of — so a column near
        // `u32::MAX` on any line but the first overflowed it: a panic in a debug
        // build, and in release a *wrapped* offset that landed on an earlier
        // line and answered about the wrong text. Saturating here leaves the
        // clamp below to do its job.
        let mut offset = usize::from(line.start()).saturating_add(lc.col as usize);
        // `to_utf8` does not clamp the column to the line: a column past the
        // end would land in the next line or past the text.
        if offset > end {
            return Some(end);
        }
        // A column inside a multi-byte character rounds up to its end, like
        // walking the line's UTF-16 units does.
        if !self.text.is_char_boundary(offset) {
            offset += self.text[offset..].chars().next()?.len_utf8();
        }
        Some(offset)
    }

    /// Where a walk over a line's characters stops: its content end, before
    /// the newline that line-index's range includes.
    fn line_end(&self, line: TextRange) -> usize {
        let end = usize::from(line.end());
        if end > usize::from(line.start()) && self.text.as_bytes().get(end - 1) == Some(&b'\n') {
            end - 1
        } else {
            end
        }
    }

    /// LSP position of a byte offset, floored to a char boundary.
    fn position(&self, offset: usize) -> Position {
        let mut offset = offset.min(usize::from(self.idx.len()));
        let lc = loop {
            match self.idx.try_line_col(TextSize::new(offset as u32)) {
                Some(lc) => break lc,
                // Inside a multi-byte character: floor to its start.
                None => offset -= 1,
            }
        };
        Position {
            line: lc.line,
            character: self
                .idx
                .to_wide(WideEncoding::Utf16, lc)
                .map_or(lc.col, |w| w.col),
        }
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

    /// The position of `needle` in `src`, `within` characters in.
    ///
    /// Locating a marker keeps a cursor tied to the text it is about. A
    /// hard-coded line and column silently points at a different token after
    /// any edit, and `occ_at`'s "ends here" fallback can resolve it to one that
    /// looks plausible, so the assertion would be about something else.
    fn at_in(src: &str, needle: &str, within: usize) -> Position {
        let start = src
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in the source"));
        Positions::new(src).position(start + within)
    }

    /// [`at_in`] against the shared [`SRC`].
    fn at(needle: &str, within: usize) -> Position {
        at_in(SRC, needle, within)
    }

    /// The tests below address `SRC` by line and column. Pin its layout so an
    /// edit to `SRC` fails this one obvious assertion instead of moving every
    /// cursor onto a different token.
    #[test]
    fn the_test_source_layout_is_pinned() {
        let lines: Vec<&str> = SRC.lines().collect();
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[1], "adult = .age >= 18");
        assert_eq!(lines[2], "q = users & where adult & agg { s = sum .age }");
        assert_eq!(lines[4], "twice = x => x + x");
        assert_eq!(lines[5], "shadow = users => users & where adult");
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range {
            start: pos(line, start),
            end: pos(line, end),
        }
    }

    /// A column far past the line is clamped, not overflowed.
    ///
    /// The byte offset is the line's start plus the column, and the sum used to
    /// be done in `u32` — the type LSP columns are made of. A column near
    /// `u32::MAX` on any line but the first overflowed it: a panic in a debug
    /// build, and in release a *wrapped* offset that pointed at an earlier line,
    /// so hover answered about text the cursor was nowhere near. An editor can
    /// send such a column, so the request must be clamped like any other
    /// out-of-range position.
    #[test]
    fn a_column_past_the_line_end_is_clamped() {
        let positions = Positions::new(SRC);
        let last = u32::MAX;
        for line in [1u32, 2, 5] {
            let end = positions.offset(pos(line, u32::MAX - 1)).expect("a line");
            // Whatever the column, the offset is on *this* line, at its end.
            let got = positions.position(end);
            assert_eq!(got.line, line, "line {line} with column {last}");
            let clamped = positions.offset(pos(line, last)).expect("a line");
            assert_eq!(clamped, end, "line {line} with column {last}");
        }
    }

    #[test]
    fn formatting_edits_only_the_lines_that_change() {
        let edits = format(&ws()).unwrap();
        let [edit] = edits.as_slice() else {
            panic!("{edits:?}")
        };
        // `q` is the definition whose layout changes and the only one in the
        // range: the five definitions around it are left out of the edit, so
        // formatting does not invalidate their tokens, folds or the cursor.
        assert_eq!(edit.range.start, pos(2, 0));
        assert_eq!(edit.range.end, pos(3, 0));
        assert_eq!(
            edit.new_text,
            "q =\n  users\n  & where adult\n  & agg { s = sum .age }\n"
        );
        // Applying them is what a client does, and it has to reproduce the
        // formatter's own output exactly.
        let formatted = crate::edits::test_support::apply(SRC, &edits);
        assert_eq!(formatted, cagara_fmt::format(SRC).unwrap().text);
        let again = Workspace::open_with(Path::new("/nonexistent/main.cagara"), formatted);
        assert!(format(&again).unwrap().is_empty());
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
        let byte = Positions::new(SRC).offset(r.start).unwrap() - SRC.find(line).unwrap();
        assert!(line.is_char_boundary(byte), "{r:?}");
    }

    /// Hover text with line breaks and indentation collapsed to spaces.
    fn one_line(h: String) -> String {
        h.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn hover_shows_types_and_overloads() {
        let ws = ws();
        // `adult` in `where adult` (line 2, column 18).
        let h = hover(&ws, at("where adult", 7)).unwrap();
        assert!(h.contains("adult : expr { age = a | b } bool"), "{h}");
        // `sum` is an overload set in the prelude.
        let h = one_line(hover(&ws, at("sum .age", 1)).unwrap());
        let (here, general) = h.split_once("defined as").unwrap();
        assert!(
            here.contains("sum : expr { age = int, id = int, name = string } int -> "),
            "{h}"
        );
        assert_eq!(general.matches("sum : ").count(), 2, "{h}");
        // Operators hover at their symbol, under their definition name.
        let h = hover(&ws, at("x + x", 2)).unwrap();
        assert!(h.contains("_+_ : "), "{h}");
        // A column reference: its value type, open where `adult` is general.
        let h = hover(&ws, at("adult = .age", 9)).unwrap();
        assert!(h.contains(".age : "), "{h}");
        // A lambda parameter shadows the table.
        let h = hover(&ws, at("shadow = users", 10)).unwrap();
        assert!(h.contains("local binding"), "{h}");
    }

    /// Hover carries the definition's own `##` text, from whichever module the
    /// definition lives in: the prelude's documentation is what a reader needs
    /// at `where`, and a local definition's is what they wrote themselves.
    ///
    /// The text is the same string `cagara doc` prints, so the editor and the
    /// reference cannot drift apart.
    #[test]
    fn hover_shows_the_definition_documentation() {
        let ws = ws();
        // A prelude definition, documented in prelude.cagara.
        let h = hover(&ws, at("sum .age", 1)).unwrap();
        assert!(h.contains("Sum of a column"), "{h}");
        // The doc comes after the types, so the signature stays the first thing
        // read.
        let types = h.find("sum : ").expect("a signature");
        let doc = h.find("Sum of a column").expect("the doc");
        assert!(types < doc, "{h}");
        // An undocumented definition says nothing extra.
        let h = hover(&ws, at("adult = .age", 9)).unwrap();
        assert!(!h.contains("---\n\n"), "no doc for a column: {h}");
    }

    /// A column is a field of a record type, so its documentation is the `##`
    /// block above the field in the signature that named it — one level below
    /// the definition that carries the table.
    const COLUMNS: &str = "## The user table.\n\
                           users : query {\n\
                           \x20   ## The primary key.\n\
                           \x20   id = int,\n\
                           \x20   ## Age in years.\n\
                           \x20   age = int\n\
                           \x20 } = table \"p\" \"users\"\n\
                           \n\
                           ## The order table.\n\
                           orders : query {\n\
                           \x20   ## The amount charged.\n\
                           \x20   amount = float,\n\
                           \x20   ## The user who placed it.\n\
                           \x20   user_id = int\n\
                           \x20 } = table \"p\" \"orders\"\n\
                           \n\
                           filtered = users & where (.age >= 18)\n\
                           renamed = users & select { a = .id } & where (.a > 0)\n\
                           joined = users & innerJoin orders (.<id == .>user_id)\n\
                           totals = orders & agg { s = sum .amount }\n";

    fn column_ws() -> Workspace {
        Workspace::open_with(
            Path::new("/nonexistent/columns.cagara"),
            COLUMNS.to_string(),
        )
    }

    /// The doc a cursor on a column shows, or `""` for "type only".
    fn column_hover(ws: &Workspace, needle: &str, within: usize) -> String {
        let h = hover(ws, at_in(COLUMNS, needle, within)).unwrap_or_default();
        h.rsplit_once("```\n\n---\n\n")
            .map_or(String::new(), |(_, doc)| doc.to_string())
    }

    /// A column's hover follows the row back to the declaration that named it:
    /// through `where`, out of a `select` argument, into an `agg`, and to the
    /// right-hand side of a join for `.>x`.
    #[test]
    fn hover_shows_a_columns_documentation_from_its_declaration() {
        let ws = column_ws();
        assert_eq!(column_hover(&ws, "where (.age", 9), "Age in years.");
        // Inside a `select`, the columns are read from the *input* row.
        assert_eq!(
            column_hover(&ws, "select { a = .id }", 13),
            "The primary key."
        );
        // `.>x` is the query the join names, `.<x` the one piped into it.
        assert_eq!(column_hover(&ws, ".>user_id", 4), "The user who placed it.");
        assert_eq!(column_hover(&ws, ".<id ==", 3), "The primary key.");
        assert_eq!(column_hover(&ws, "sum .amount", 5), "The amount charged.");
    }

    /// Hovering the field where it is declared shows its block: a field has no
    /// type of its own to report (the line beside the cursor says it), so the
    /// documentation is the whole answer.
    #[test]
    fn hover_at_a_field_declaration_shows_its_documentation() {
        let ws = column_ws();
        let h = hover(&ws, at_in(COLUMNS, "id = int", 1)).expect("a hover");
        assert_eq!(h, "The primary key.");
        // A field with no block says nothing, rather than an empty tooltip.
        let src = "users : query {\n    ## Key.\n    id = int,\n    name = string\n  } = table \"p\" \"t\"\n\n\
                   q = users\n";
        let ws = Workspace::open_with(
            Path::new("/nonexistent/plain-fields.cagara"),
            src.to_string(),
        );
        assert_eq!(hover(&ws, at_in(src, "name = string", 1)), None);
    }

    /// An alias is not a declaration. `t = users` says nothing about the
    /// columns, so the walk follows its body rather than stopping at a
    /// definition with no record to read; and the `&…` shorthands carry the
    /// step inline, so their arguments read the piped query's row like any
    /// other stage's.
    #[test]
    fn an_alias_and_a_shorthand_still_reach_the_declaration() {
        let src = "## T.\nusers : query {\n    ## Age.\n    age = int\n  } = table \"p\" \"t\"\n\n\
                   alias = users\n\
                   via_alias = alias & where (.age > 0)\n\
                   shorthand = users &? (.age > 0)\n\
                   sorted = users &. [asc .age]\n\
                   unioned = users &| users & where (.age > 0)\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/alias.cagara"), src.to_string());
        for needle in [
            "via_alias = alias & where (.age > 0)",
            "shorthand = users &? (.age > 0)",
            "sorted = users &. [asc .age]",
            "unioned = users &| users & where (.age > 0)",
        ] {
            let h = hover(
                &ws,
                at_in(src, needle, needle.find(".age").unwrap_or(0) + 1),
            )
            .unwrap_or_default();
            assert!(h.contains("Age."), "`{needle}` lost the doc: {h}");
        }
    }

    /// A column with no declaration behind it shows its type and nothing else:
    /// `select { a = .id }` computed `a`, so no block describes it, and saying
    /// something would mean guessing which table's `id` it resembles.
    #[test]
    fn a_computed_column_has_no_documentation() {
        let ws = column_ws();
        assert_eq!(column_hover(&ws, "where (.a >", 9), "");
        // The type is still there, which is what the hover is for.
        let h = hover(&ws, at_in(COLUMNS, "where (.a >", 9)).unwrap();
        assert!(h.contains(".a : expr r int"), "{h}");
    }

    /// A column that `prefix` renamed is no longer the declared one: the doc
    /// would be about a name that the record does not have.
    #[test]
    fn a_renamed_column_has_no_documentation() {
        let src = "## T.\nusers : query {\n    ## Key.\n    id = int\n  } = table \"p\" \"t\"\n\n\
                   q = users & prefix \"u_\" & where (.u_id > 0)\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/prefixed.cagara"), src.to_string());
        let h = hover(&ws, at_in(src, "where (.u_id", 9)).unwrap();
        assert!(!h.contains("Key."), "{h}");
    }

    #[test]
    fn hover_shows_the_type_at_the_use() {
        let src = "users : query { id = int, name = string } = table \"p\" \"users\"\n\
                   q = users & select { n = .name }\n\
                   twice = x => x + x\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        // `select` at its use: instantiated, then the general scheme.
        let h = one_line(hover(&ws, at_in(src, "& select", 3)).unwrap());
        let (here, general) = h.split_once("defined as").unwrap();
        assert!(
            here.contains(
                "select : expr { id = int, name = string } (row { n = string }) -> query { id = int, name = string } -> query { n = string }"
            ),
            "{h}"
        );
        assert!(
            general.contains("select : expr r (row s) -> query r -> query s"),
            "{h}"
        );
        // A monomorphic use shows its type once.
        let h = hover(&ws, at_in(src, "q = users", 4)).unwrap();
        assert!(
            !h.contains("defined as") && h.contains("users : query { id = int, name = string }"),
            "{h}"
        );
        // A lambda parameter shows its inferred type.
        let h = hover(&ws, at_in(src, "=> x", 3)).unwrap();
        assert!(
            h.contains("x : expr ") && h.contains("local binding"),
            "{h}"
        );
    }

    #[test]
    fn hover_shows_columns_and_literals() {
        let src = "users : query { id = int, name = string, score = float } = table \"p\" \"users\"\n\
                   q = users & where (.score >= 18 && .name == \"a\") & select { s = .score, k = 1 }\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let at = |needle: &str, within: usize| {
            hover(&ws, at_in(src, needle, within)).unwrap_or_default()
        };
        // `.score` as the expression type it has: a column is row-phase, so it
        // is `expr r float`, not the bare value type.
        let h = at("where (.score", 8);
        assert!(h.contains(".score : expr r float"), "{h}");
        // `18` lifted to the column's type.
        let h = at(">= 18", 3);
        assert!(h.contains("18 : float"), "{h}");
        let h = at("&& .name", 3);
        assert!(h.contains(".name : expr r string"), "{h}");
        let h = at("== \"a\"", 3);
        assert!(h.contains("\"a\" : string"), "{h}");
        // A literal in a record keeps its own type.
        let h = at("k = 1", 4);
        assert!(h.contains("1 : int"), "{h}");
    }

    /// A column hovers as the expression type it has, not as its value type.
    ///
    /// A column is not a bare value: it is row-phase, which is what every
    /// `expr r a` parameter accepts, and writing `string` for it hides both the
    /// phase and the row — a reader copying the hover into a helper's signature
    /// wrote `string -> string`, which the checker then refused. `r` is the row
    /// variable a signature would name, so the hover is the shape the reader has
    /// to write; the concrete row is deliberately elided, because printing it
    /// repeats the whole table for every column and puts the column first,
    /// which is not the row's order.
    ///
    /// A literal is *not* wrapped: it may be a label rather than an expression
    /// (`limit 2`, `prefix "u_"`), where `expr r …` would be wrong.
    #[test]
    fn a_column_hovers_as_an_expression_over_the_row() {
        let src = "users : query { id = int, name = string, score = maybe float } = table \"p\" \"users\"\n\
                   o : query { user_id = int } = table \"p\" \"o\"\n\
                   q = users & select { n = upper .name, v = coalesce 0.0 .score } & limit 2\n\
                   j = users & innerJoin o (.<id + 1 == .>user_id + 1)\n\
                   p = users & prefix \"u_\"\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        // The signature inside the popup, with its fences and the line breaks
        // the width-driven layout adds taken out.
        let at = |needle: &str, within: usize| {
            one_line(hover(&ws, at_in(src, needle, within)).unwrap_or_default())
                .replace("```cagara", "")
                .replace("```", "")
                .trim()
                .to_string()
        };

        assert_eq!(at("upper .name", 8), ".name : expr r string");
        // A compound value type is parenthesised, as the printer spells it, so
        // the hover parses as the type it means.
        assert_eq!(
            at("coalesce 0.0 .score", 15),
            ".score : expr r (maybe float)"
        );
        // A side-qualified column is an expression over the joined row.
        assert_eq!(at(".<id + 1", 2), ".<id : expr (join r s) int");
        // A literal stays the type it was lifted to, and so does a label: the
        // `2` here is `limit`'s count, not an expression over the row.
        assert_eq!(at("limit 2", 6), "2 : int");
        // The `"u_"` affix has no hover to give: it is a label, not a value.
        assert!(hover(&ws, at_in(src, "prefix \"u_\"", 8)).is_none());
    }

    /// A literal body is shown at the type its definition's signature gives it,
    /// not at the default kind it was inferred with.
    ///
    /// `infer` records `"2025-12-31"` as `string`; the signature is what lifts
    /// it to `date`. A definition body reaches its type by `coerce` rather than
    /// through `check`, so the lift went unrecorded and the definition said
    /// `date` while its own body said `string`. The same literal against a
    /// `date` column already read back as `date`, which is what made the
    /// disagreement visible.
    #[test]
    fn hover_shows_a_literal_at_its_signature_type() {
        let src = "d : date = \"2025-12-31\"\nf : float = 18\nq = 1\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let at = |needle: &str, within: usize| {
            hover(&ws, at_in(src, needle, within)).unwrap_or_default()
        };
        let h = at("\"2025-12-31\"", 3);
        assert!(h.contains("\"2025-12-31\" : date"), "{h}");
        let h = at("= 18", 2);
        assert!(h.contains("18 : float"), "{h}");
    }

    /// A `case` arm is walked like any other expression, so hover and
    /// navigation work inside both halves of an arm — the condition and the
    /// value it selects.
    #[test]
    fn case_arms_are_reachable_by_hover_and_definition() {
        let src = "users : query { id = int, score = float } = table \"p\" \"users\"\n\
                   adult = .score >= 18\n\
                   q = users & select { b = case { adult => .score; true => 0.0 } }\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        // The condition is a name use, resolved to its definition.
        let h = hover(&ws, at_in(src, "adult =>", 2)).unwrap_or_default();
        assert!(h.contains("adult"), "{h}");
        let d = definition(&ws, at_in(src, "adult =>", 2));
        assert_eq!(d.len(), 1, "{d:?}");
        // The value half is an expression over the row, so its column hovers.
        let h = hover(&ws, at_in(src, "=> .score", 4)).unwrap_or_default();
        assert!(h.contains(".score : expr r float"), "{h}");
        // Both halves are recorded as references to what they name.
        let rs = references(&ws, at_in(src, "adult =>", 2), false);
        assert!(!rs.is_empty(), "the condition is a use of `adult`");
    }

    /// A `let` binding is a local name for hover, go-to-definition,
    /// references, and completion. The AST desugars it to a lambda whose span
    /// starts at the binding name and ends at the `let`, which is the scope the
    /// walk needs.
    #[test]
    fn let_bindings_hover_and_navigate() {
        let src = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
                   q = users & select { x = let age2 = .age in age2 + 1 }\n";
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let use_pos = at_in(src, "age2 + 1", 0);
        let h = hover(&ws, use_pos).unwrap_or_default();
        assert!(h.contains("age2") && h.contains("local binding"), "{h}");
        assert!(h.contains("int"), "{h}");
        let d = definition(&ws, use_pos);
        assert_eq!(d.len(), 1, "{d:?}");
        let decl = src.find("let age2").expect("binding text") + 4;
        assert_eq!(d[0].1.start, Positions::new(src).position(decl));
        let rs = references(&ws, use_pos, true);
        assert_eq!(rs.len(), 2, "binding and use: {rs:?}");
        // Completion inside the body sees the binding.
        let p = Positions::new(src).position(src.find("in age2").expect("body text") + 3);
        let labels: Vec<String> = completion(&mut ws, p)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(labels.iter().any(|l| l == "age2"), "{labels:?}");
    }

    #[test]
    fn hover_breaks_long_types() {
        let src = "users : query { id = int, name = string, email = string, created = timestamp, score = float } = table \"p\" \"users\"\n\
                   q = users & where (.score >= 1)\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        let h = hover(&ws, at_in(src, "q = users", 4)).unwrap();
        assert!(h.lines().all(|l| l.chars().count() <= HOVER_WIDTH), "{h}");
        assert!(h.contains("users : query {\n  id = int,\n"), "{h}");
        // `where` at its use: one arrow per line.
        let h = hover(&ws, at_in(src, "& where", 3)).unwrap();
        assert!(h.contains("\n  -> query {"), "{h}");
    }

    #[test]
    fn definition_points_at_the_name() {
        let ws = ws();
        let d = definition(&ws, at("where adult", 7));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, range(1, 0, 5));
        // Prelude definitions have no file.
        assert!(definition(&ws, at("sum .age", 1)).is_empty());
        // A parameter use jumps to the parameter.
        let d = definition(&ws, at("shadow = users", 10));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, range(5, 9, 14));
    }

    /// An annotated parameter is a name like any other: `(x : ty)` puts the
    /// name after a `(` and a `:`, which used to be the one place a parameter
    /// could not be found.
    #[test]
    fn an_annotated_parameter_is_a_name_in_scope() {
        let src = "users : query { id = int, name = string } = table \"p\" \"users\"\n\
                   twice = (x : expr r int) => x + x\n\
                   q = users & select { n = twice .id }\n";
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.to_string());
        // A use jumps to the name inside the parentheses, not to the `(`.
        let d = definition(&ws, at_in(src, "x + x", 0));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, range(1, 9, 10));
        // Both uses and the declaration are one name.
        assert_eq!(
            highlights(&ws, at_in(src, "x + x", 0))
                .iter()
                .map(|(r, _)| *r)
                .collect::<Vec<_>>(),
            vec![range(1, 9, 10), range(1, 28, 29), range(1, 32, 33)]
        );
        // Hovering a use shows the type the annotation gave it.
        let h = one_line(hover(&ws, at_in(src, "x + x", 0)).unwrap());
        assert!(h.contains("x : ") && h.contains("int"), "{h}");
        // Hovering the declaration itself names what it is.
        let h = one_line(hover(&ws, at_in(src, "x : expr", 0)).unwrap());
        assert!(h.contains("local binding"), "{h}");
        // The parameter is offered while completing the body.
        let labels = completion(&mut ws, at_in(src, "x + x", 0))
            .into_iter()
            .map(|i| i.label)
            .collect::<Vec<_>>();
        assert!(labels.iter().any(|l| l == "x"), "{labels:?}");
    }

    #[test]
    fn references_respect_shadowing() {
        let ws = ws();
        // `users` from its use in `q`: the definition and two uses, not the
        // lambda parameter in `shadow`.
        let rs = references(&ws, at("q = users", 4), true);
        assert_eq!(rs, vec![range(0, 0, 5), range(2, 4, 9), range(3, 6, 11)]);
        let rs = references(&ws, at("q = users", 4), false);
        assert_eq!(rs, vec![range(2, 4, 9), range(3, 6, 11)]);
        // `x` in `twice`: the parameter and both uses.
        let hs = highlights(&ws, at("=> x", 3));
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

    /// A `case` arm takes the same expressions as anywhere else, so completion
    /// works on both halves of an arm.
    ///
    /// The two halves can report the row's columns in different *orders*: the
    /// row a `case` sees is built in the order its arms first mention columns
    /// (`.age` in the condition, then `.id` in the value), where the table's
    /// own order is `id, age`. Both list the same columns, so the ordering is
    /// pinned here rather than left implicit.
    #[test]
    fn column_completion_inside_a_case_arm() {
        let src = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
                   a = users & select { b = case { .age >= 18 => .id; true => .id } }\n";
        let at = |needle: &str, within: usize| at_in(src, needle, within);
        // The condition half completes from the input row, in table order.
        assert_eq!(fields(src, at(".age >= 18", 3)), ["id", "age"]);
        // The value half completes from the same row, in the order the arms
        // first named its columns.
        assert_eq!(fields(src, at("18 => .id", 8)), ["age", "id"]);
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

    /// Columns reach completion through a key stage whatever the pipeline
    /// does in between.
    ///
    /// A `prefix` renames the row it is handed, and the rename travels as a
    /// `keyMap` term. A stage between the source and the rename that also
    /// constrains the row — `order`'s sort key, `where`'s predicate — used to
    /// leave the row unrenamed by the time the record was checked, so the
    /// columns offered were the *pre-rename* ones and the renamed columns the
    /// user had just written were missing.
    #[test]
    fn column_completion_through_a_key_stage() {
        let tail = "  & prefix \"lit_\"\n\
                    &= {id = .lit_id, id44 = .lit_label <> .lit_label <> .}\n";
        let head = "users : query { id = int, name = string, age = int, active = bool } = \
                    table \"public\" \"users\"\n\
                    adults = users\n\
                    & where (.age >= 18 && .active)\n\
                    & select { id = .id, label = upper .name, next_age = .age + 1, id2 = .id }\n";
        // The pipeline with, and without, the ordering and paging stages: the
        // renamed columns must be offered either way.
        for stages in ["", "  & order [asc .label]\n  & limit 10\n"] {
            let src = format!("{head}{stages}{tail}");
            let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), src.clone());
            let line = src.lines().count() as u32 - 1;
            let col = src.lines().nth(line as usize).unwrap().chars().count() as u32 - 1;
            let labels: Vec<String> = completion(&mut ws, pos(line, col))
                .into_iter()
                .map(|i| i.label)
                .collect();
            assert_eq!(
                labels,
                ["lit_label", "lit_id", "lit_next_age", "lit_id2"],
                "stages {stages:?}"
            );
        }
    }

    #[test]
    fn positions_round_trip() {
        let t = "ab\né x\n";
        let p = Positions::new(t);
        for off in [0, 1, 3, 5, 6] {
            assert_eq!(p.offset(p.position(off)), Some(off));
        }
        assert_eq!(p.position(6), pos(1, 2));
        // A byte offset inside a multi-byte character floors to its start
        // rather than panicking or landing on the next character.
        assert_eq!(p.position(4), p.position(3));
        assert_eq!(p.offset(p.position(4)), Some(3));
        // Past the end clamps to the end of the text.
        assert_eq!(p.offset(p.position(100)), Some(t.len()));
    }

    /// The probe is spliced into whichever definition holds the cursor, but
    /// checking a module runs every definition. The recorded row must belong
    /// to the definition under the cursor, not to whichever definition
    /// happened to be checked last.
    #[test]
    fn the_probe_belongs_to_the_definition_being_completed() {
        // The cursor sits after `n = .` inside `c`, and `helper` follows it.
        let afterwards = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
                          c = users & select { n = .\n\
                          helper = u => u & where (.age > 0)\n";
        assert_eq!(fields(afterwards, pos(1, 26)), ["id", "age"]);

        // The same file with the definitions swapped: `c` is still the
        // definition under the cursor, and still the one that knows the row.
        let before = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
                      helper = u => u & where (.age > 0)\n\
                      c = users & select { n = .\n";
        assert_eq!(fields(before, pos(2, 26)), ["id", "age"]);
    }
}
