//! Diagnostics, hover, definitions, references, highlights, symbols,
//! completion, and formatting for the root file of a workspace. Names are resolved on the
//! AST, so a lambda parameter shadows a top-level name of the same spelling.
//! LSP positions count UTF-16 code units; the workspace uses byte offsets.

use cagara_hir::check::{check_reachable, TypeCheck, PROBE_FIELD};
use cagara_hir::compile_diagnostics;
use cagara_hir::workspace::{Binding, Diag, DiagKind, Severity, Workspace};
use cagara_syntax::ast::{Expr, ExprKind, Side, Span};
use cagara_syntax::docs::Docs;
use line_index::{LineIndex, TextRange, TextSize, WideEncoding};
use lsp_types::{
    CompletionItem, CompletionItemKind, DocumentHighlightKind, DocumentSymbol, Position, Range,
    SymbolKind, TextEdit,
};
use lsp_types::{FoldingRange, FoldingRangeKind};
use std::collections::HashSet;
use std::path::PathBuf;

/// A diagnostic as the language server publishes it.
///
/// The `kind` is the diagnostic's machine-readable identity — what the client
/// sees as its `code` — and it travels with the message from where the
/// diagnostic was *produced*. It is deliberately not derived from the message:
/// the message is wording, and a client filtering on a code must not be
/// affected by rewording one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Report {
    pub range: Range,
    pub message: String,
    pub severity: Severity,
    pub kind: DiagKind,
}

/// Every diagnostic in the root file, for a document with no configured dialect.
///
/// ANSI is the fallback — see [`diagnostics_with`] — and it is what every caller that
/// does not know the document's target should ask for.
pub fn diagnostics(ws: &Workspace) -> Vec<Report> {
    diagnostics_with(ws, cagara_sql::Dialect::Ansi)
}

/// Every diagnostic in the root file: loading (syntax, imports, overloads),
/// type errors, and compilation errors — lowered for `dialect`.
///
/// `dialect` is the document's configured target (`cagara.dialect`, from the
/// client's `initializationOptions` or `workspace/didChangeConfiguration`). It affects
/// exactly one thing: the refusals that `cagara-sql` makes *about a target* — a
/// `groups` window frame on MySQL, a `list` on a dialect with no array type. The
/// editor and `cagara --dialect` therefore agree about the same file, which they did
/// not when the server always asked ANSI: a file written for MySQL looked clean in the
/// editor and failed on the command line.
///
/// ANSI remains the fallback when no target is configured, and it is the right one for
/// a different reason than before: it is the most permissive target — `FULL JOIN`,
/// grouping sets, all three frame units, arrays, an exact decimal — so a document whose
/// target nobody stated gets refusals that are about the *program* rather than about a
/// target's spelling, and a reader is never shown a squiggle that is only true of a
/// target they did not choose.
///
/// The cost of lowering at all is about a tenth of a check: measured on 80 queries,
/// `check` took 68 ms and lowering all of them took 6.5 ms in a debug build.
pub fn diagnostics_with(ws: &Workspace, dialect: cagara_sql::Dialect) -> Vec<Report> {
    let compilation_diagnostics = compile_diagnostics(ws);
    let root = ws.modules[ws.root].path.display().to_string();
    // The checker is not the last thing that can refuse. `cagara-sql` builds the
    // statement, and it refuses some programs the checker accepted — a `distinct`
    // after an `order` on an expression the projection dropped is one, and it is a
    // real error with a message telling the author what to write. Without this the
    // editor showed nothing for a file `cagara --only` rejects, so "it compiles in my
    // editor" was false in exactly the case the author needed to know about.
    let lowering = lowering_diagnostics(ws, dialect);
    // Deduped through a set, not `Vec::contains`: a file being typed has as
    // many diagnostics as it has errors, and comparing each against every one
    // already collected made publishing quadratic in them. The compiler's own
    // collector dedupes the same way (`compile::push_unique`).
    let mut out: Vec<Report> = Vec::new();
    let mut seen: HashSet<Report> = HashSet::new();
    for d in compilation_diagnostics.iter().filter(|d| d.path == root) {
        let item = Report {
            range: diag_range(d),
            message: d.message.clone(),
            severity: d.severity,
            kind: d.kind,
        };
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    for item in lowering {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// What the SQL lowering refuses about the root file, positioned at the stage.
///
/// The dialect comes from the caller — the document's `cagara.dialect` setting, or ANSI
/// when none is configured. Which target is asked matters, and both answers are right
/// in their own case:
///
/// * **ANSI, the fallback,** is the permissive target: it has `FULL JOIN`, grouping
///   sets, all three frame units, arrays and an exact decimal, so what is left are the
///   refusals about the *relation* rather than about a target's spelling. A document
///   whose target nobody stated must not be painted with a refusal that is only true of
///   some other target — that is a squiggle about a choice the user never made.
/// * **The configured target,** once there is one, is the honest question: the editor
///   then reports exactly what `cagara --dialect <that>` reports, so a file that shows
///   clean in the editor no longer fails on the command line. Before the setting
///   existed, a MySQL-targeted file with a `groups` frame looked fine in the editor and
///   was refused by the compiler.
///
/// It costs about a tenth of a check: measured on 80 queries, `check` took 68 ms and
/// lowering all of them to SQL took 6.5 ms in a debug build.
///
/// Only the root module is reported, for the same reason `diagnostics` filters on the
/// root path: an imported file is its own document, and reporting its errors against
/// this one would put them in a file the user is not looking at.
fn lowering_diagnostics(ws: &Workspace, dialect: cagara_sql::Dialect) -> Vec<Report> {
    let tc = check_reachable(ws);
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
    let mut out = Vec::new();
    for (name, result) in cagara_hir::root_queries_checked(ws, &tc) {
        let Ok(rel) = result else {
            // The checker's own diagnostic is already reported above, in its own
            // words and at its own position.
            continue;
        };
        let Err((loc, message)) = cagara_sql::compile_located(
            &rel,
            cagara_sql::Options {
                dialect,
                pretty: false,
                optimize: false,
            },
        ) else {
            continue;
        };
        let range = match loc {
            // A `Loc` in another module is not this document; the definition's own
            // file reports it when it is open.
            Some(l) if l.module == ws.root => {
                root_range(&p, l.span.start as usize, l.span.end as usize)
            }
            Some(_) => continue,
            // Nothing in the relation carried a span. The refusal is still true, so it
            // is reported at the top of the file rather than dropped — a diagnostic in
            // the wrong place is worse than none, but a missing error is worse than a
            // coarse one.
            None => Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        };
        out.push(Report {
            range,
            message: format!("error in `{name}`: {message}"),
            severity: Severity::Error,
            // A refusal of the SQL lowering. Today these are about the
            // *relation*, not the target (see this function's note on ANSI);
            // a target-specific refusal would carry the same kind, because what
            // it reports is the same thing — this construct cannot be written.
            kind: DiagKind::Lowering,
        });
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

/// Every resolved name occurrence in `module`, in source order.
///
/// Desugared names that are not spelled in the source (`negate` for `-x`) are
/// skipped; operators count where their symbol is written.
///
/// The module is a parameter rather than always the root because a *reference* to
/// a definition can be written in a file that imports it, and the cursor's file is
/// the only one whose positions are being asked about. Everything that answers about
/// the cursor (`hover`, `definition`, `highlights`, `completion`) passes the root;
/// `references` walks the reachable modules one at a time and converts each with its
/// own `Positions`, since byte offsets mean nothing across two texts.
fn occurrences(ws: &Workspace, module: usize) -> Vec<Occ> {
    let md = &ws.modules[module];
    let mut w = Walk {
        ws,
        module,
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
    /// The module whose text the spans are offsets into.
    module: usize,
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
        let md = &ws.modules[self.module];
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
            // An ascription's type is not an expression, so only the value is
            // walked; a column inside `(e : ty)` is still a use of that column.
            ExprKind::Ascribe { expr, .. } => self.expr(expr),
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
/// row ?a signature fixed. That definition's declared record type is where the
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
        // `q & st`: the piped query decides the row, and every
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
    /// `query ?r` — is a *step*, not a declaration: it says nothing about the
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

/// A stable slug for a diagnostic, so a client can filter or link on it.
///
/// The *message* is the language's wording and is meant to change as the
/// wording improves; the code is the identity. So the code comes from
/// [`DiagKind`], which the producer set where it knew the category — never from
/// the text. Reading it off the message is what this used to do, and it made
/// rewording a message a silent change to machine-readable behaviour: the
/// `unknown-column` arm matched `"no column"`, so an improvement to that
/// sentence would have quietly reclassified every missing-column diagnostic as
/// the catch-all `error`.
///
/// The mapping is deliberately coarse — one code per kind of mistake, not per
/// sentence — because a code that changes whenever a message is reworded is
/// worse than none.
pub(crate) fn diagnostic_code(kind: DiagKind) -> &'static str {
    kind.code()
}

/// Whether an expression is a pipeline step. `&` is the only spelling — the
/// per-stage shorthands were removed, so this is one name and not eleven.
fn is_pipeline(name: Option<&str>) -> bool {
    matches!(name, Some("_&_"))
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
    let occs = occurrences(ws, ws.root);
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
/// A column is spelled as the expression type it has — `expr ?r string`, or
/// `expr (join r s) int` for a side-qualified one — because a column is not a
/// bare value: it is row-phase, it is what every `expr ?r a` parameter accepts,
/// and the bare value type hides both. `r` is the input row of the enclosing
/// stage, the same name a signature gives it; printing the concrete row instead
/// would repeat the whole table for every column and put the column first,
/// which is not the row's order.
///
/// A literal keeps the type it was lifted to (`18` against a `float` column is
/// `float`) and is not wrapped: a literal may be a *label* rather than an
/// expression (`limit 2`, `prefix "u_"`), where `expr ?r …` would be wrong.
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

/// What `signatureHelp` shows: the signatures of the name being applied, and which
/// argument the cursor is on.
///
/// `hover` already answers "what is this name". This answers the *positional* question a
/// signature help popup exists for: `arrayAgg`'s first argument is the expression, its
/// second is nothing — but `toDecimal`'s first is the precision, its second the scale and
/// its third the value, and a reader who has typed two arguments wants to know what the
/// third is.
///
/// The parameter is found by walking the application spine and asking which argument
/// *contains* the cursor. That is deliberately syntactic: the checker has no notion of an
/// argument being "in progress", and a half-typed one does not type-check at all.
pub fn signature_help(ws: &Workspace, pos: Position) -> Option<(Vec<String>, usize)> {
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
    let offset = p.offset(pos)?;
    // The innermost application whose argument list contains the cursor.
    let mut best: Option<(&Expr, usize)> = None;
    for d in &md.module.defs {
        app_at(&d.body, offset, &mut best);
    }
    let (call, active) = best?;
    let ExprKind::App(f, _) = &call.kind else {
        return None;
    };
    // The name being applied, resolved so an alias and an overload set both work.
    let ExprKind::Name(name) = &f.kind else {
        return None;
    };
    let tc = check_reachable(ws);
    let occs = occurrences(ws, ws.root);
    let o = occs
        .iter()
        .find(|o| o.start <= f.span.start as usize && (f.span.end as usize) <= o.end)?;
    let Target::Global(binding) = &o.target else {
        return None;
    };
    let defs: Vec<(usize, usize)> = match binding {
        Binding::Def(m, i) => vec![(*m, *i)],
        Binding::Overloads(m, is) => is.iter().map(|&i| (*m, i)).collect(),
        Binding::Module(_) => return None,
    };
    let sigs: Vec<String> = defs
        .into_iter()
        .filter_map(|(m, i)| {
            let ty = tc.type_of(m, i)?;
            Some(sig(name, ty))
        })
        .collect();
    (!sigs.is_empty()).then_some((sigs, active))
}

/// The innermost `App` whose arguments contain `offset`, and which one.
///
/// Innermost because `f (g x)` puts the cursor in `g`'s argument list, not `f`'s, and the
/// popup should describe the call the reader is typing. An argument's span is used rather
/// than the parentheses, because a half-typed argument may have no closing one.
fn app_at<'a>(e: &'a Expr, offset: usize, best: &mut Option<(&'a Expr, usize)>) {
    match &e.kind {
        ExprKind::App(f, args) => {
            // Children first, and a match found deeper **wins over** this one. The
            // outermost application's argument spans contain everything inside them, so
            // `q = t & select { v = toDecimal 10 2 1.5 }` has `_&_` as a match at every
            // offset — and the first version, which took the outermost, reported `_&_`'s
            // signature for a cursor sitting inside `toDecimal`.
            //
            // `best.is_none()` is what makes depth win: once a deeper call has claimed
            // the offset, the enclosing application leaves it alone.
            app_at(f, offset, best);
            for a in args {
                app_at(a, offset, best);
            }
            if best.is_some() {
                return;
            }
            if let Some(i) = args
                .iter()
                .position(|a| (a.span.start as usize) <= offset && offset <= (a.span.end as usize))
            {
                *best = Some((e, i));
            }
        }
        ExprKind::Proj(inner, _) => app_at(inner, offset, best),
        ExprKind::Lambda(_, body) => app_at(body, offset, best),
        ExprKind::Record(fs) => fs.iter().for_each(|(_, v)| app_at(v, offset, best)),
        ExprKind::List(xs) => xs.iter().for_each(|x| app_at(x, offset, best)),
        ExprKind::Case(arms) => arms.iter().for_each(|(c, v)| {
            app_at(c, offset, best);
            app_at(v, offset, best);
        }),
        _ => {}
    }
}

/// The row ?a column reference is an expression over: the input row, or the
/// joined row for the side-qualified form.
fn row_of(side: Side) -> &'static str {
    // The row is a type variable, so it carries the `?` sigil like every other
    // one: the hover is meant to read as something a signature could contain.
    match side {
        Side::Single => "?r",
        Side::Left | Side::Right => "(join ?r ?s)",
    }
}

/// `expr`'s value argument, parenthesised when it is compound — the same
/// parenthesisation the checker's printer applies, so `expr ?r (maybe float)`
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
        ExprKind::Ascribe { expr, .. } => atom_at(text, expr, offset, found),
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
    let occs = occurrences(ws, ws.root);
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

/// The token types this server reports, in the order its indices refer to.
///
/// Only what a grammar cannot know is here. A TextMate grammar already highlights
/// strings, numbers and comments — they are lexical, and the editor does that well.
/// What it cannot know is whether `users` in `.name` is a table, a column or a
/// parameter: that is *resolution*, and it is the only thing worth sending.
pub const TOKEN_TYPES: &[&str] = &["function", "variable", "parameter", "namespace", "type"];

const TY_FUNCTION: u32 = 0;
const TY_VARIABLE: u32 = 1;
const TY_PARAMETER: u32 = 2;
const TY_NAMESPACE: u32 = 3;
const TY_TYPE: u32 = 4;

/// One semantic token, in source order, before delta encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub line: u32,
    pub start: u32,
    /// Length in UTF-16 units, which is what the protocol counts.
    pub len: u32,
    pub kind: u32,
}

/// The resolved names in the root file, classified.
///
/// The classification is `occurrences`' own: a lambda parameter is a `parameter`, an
/// import alias is a `namespace`, a definition's name or use is a `function` when its
/// type is one and a `variable` when it is not, and a name that resolves to nothing —
/// a column, a record field — is a `property`. The last one is the common case and the
/// reason this exists at all: a column is not bound, so nothing but the checker knows
/// it is a column rather than a name that happens to be spelled that way.
pub fn semantic_tokens(ws: &Workspace) -> Vec<Token> {
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
    let tc = check_reachable(ws);
    let mut out: Vec<Token> = Vec::new();
    for o in occurrences(ws, ws.root) {
        let kind = match &o.target {
            Target::Local(_) => TY_PARAMETER,
            Target::Global(Binding::Module(_)) => TY_NAMESPACE,
            Target::Global(b @ (Binding::Def(..) | Binding::Overloads(..))) => {
                // An overload set is one name with several signatures; the first is
                // enough to say whether it is applied, which is all this decides.
                let (m, first) = match b {
                    Binding::Def(m, i) => (*m, Some(*i)),
                    Binding::Overloads(m, is) => (*m, is.first().copied()),
                    Binding::Module(_) => unreachable!("matched on Def or Overloads"),
                };
                let rendered = first.and_then(|i| tc.type_of(m, i));
                match rendered {
                    Some(t) if t.contains("->") => TY_FUNCTION,
                    Some(_) => TY_VARIABLE,
                    // No recorded type: the definition is broken, and guessing would
                    // colour it wrongly with confidence. `type` is the neutral choice.
                    None => TY_TYPE,
                }
            } // `Binding` has exactly three variants and all three are handled above, so
              // there is no fallback. A column is *not* an occurrence at all — it has no
              // binding — so it never reaches this function, which is why there is no
              // "column" token type: the grammar colours those, and a type nothing can
              // produce would be a lie in the legend.
        };
        let start = p.position(o.start);
        let end = p.position(o.end);
        // A name does not span lines, so the length is the difference on one line.
        out.push(Token {
            line: start.line,
            start: start.character,
            len: end.character.saturating_sub(start.character),
            kind,
        });
    }
    // The protocol requires source order, and `occurrences` returns them in the order
    // it resolved them, which is not the same thing.
    out.sort_by_key(|t| (t.line, t.start));
    out
}

/// Delta-encode tokens the way `SemanticTokens.data` wants them.
///
/// Each token is `[deltaLine, deltaStart, length, tokenType, tokenModifiers]`, where
/// `deltaStart` is relative to the previous token *on the same line* and absolute when
/// the line changed. Getting that wrong is the classic semantic-token bug: the editor
/// paints the file correctly for the first line and progressively further off after
/// that.
pub fn encode_tokens(tokens: &[Token]) -> Vec<[u32; 5]> {
    let mut out = Vec::with_capacity(tokens.len());
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for t in tokens {
        let delta_line = t.line - prev_line;
        let delta_start = if delta_line == 0 {
            t.start - prev_start
        } else {
            t.start
        };
        out.push([delta_line, delta_start, t.len, t.kind, 0]);
        prev_line = t.line;
        prev_start = t.start;
    }
    out
}

/// The regions of the root file an editor should be able to fold.
///
/// Three kinds, because they are the three things a reader collapses for different
/// reasons:
///
/// - a **definition** whose body runs over more than one line. The range starts at the
///   definition's name, not at its type: folding `q : query { … } = …` should keep `q`
///   visible, which is what an outline of the file is for.
/// - a **`##` doc block**, which is prose and usually the longest thing in the file.
///   `FoldingRangeKind::Comment` is what lets an editor offer "fold all comments"
///   separately from "fold all definitions".
/// - a **`catalog` declaration**, which is a schema's worth of tables in one block.
///
/// A one-line definition gets no range. An editor given a range whose start and end are
/// the same line either ignores it or folds a line into itself, and neither is a fold
/// the reader asked for.
pub fn folding_ranges(ws: &Workspace) -> Vec<FoldingRange> {
    let md = &ws.modules[ws.root];
    let text = &md.text;
    let p = Positions::new(text);
    let mut out: Vec<FoldingRange> = Vec::new();
    for d in &md.module.defs {
        let start = p.position(d.span.start as usize);
        let end = p.position(d.span.end as usize);
        if end.line > start.line {
            out.push(FoldingRange {
                start_line: start.line,
                start_character: Some(start.character),
                end_line: end.line,
                // The body's last line is often `}`, and folding *through* the brace
                // reads better than folding up to it: the closing line is what tells
                // the reader the block ended.
                end_character: Some(end.character),
                kind: None,
                collapsed_text: None,
            });
        }
    }
    // A `catalog "schema" { … }` block. It does not survive lowering as one node —
    // each entry becomes its own table definition — so the *parse tree* is where the
    // block still exists as a block. Re-parsing here is cheap (the file was parsed to
    // open it) and keeps this function independent of how the AST is shaped.
    {
        let parse = cagara_syntax::parse(text);
        for node in parse.syntax().descendants() {
            if node.kind() != cagara_syntax::SyntaxKind::CatalogDecl {
                continue;
            }
            let r = node.text_range();
            let start = p.position(u32::from(r.start()) as usize);
            let end = p.position(u32::from(r.end()) as usize);
            if end.line > start.line {
                out.push(FoldingRange {
                    start_line: start.line,
                    start_character: Some(start.character),
                    end_line: end.line,
                    end_character: Some(end.character),
                    kind: None,
                    collapsed_text: None,
                });
            }
        }
    }
    // The `##` blocks. A block is a run of lines each starting with `##` after
    // indentation; `starts_its_line` is the same rule `cagara_syntax::docs` uses to
    // decide what documents what, so a block folded here is a block that is read as
    // documentation there.
    let mut run: Option<(usize, usize)> = None;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        // The doc marker, from the one place it is defined: the LSP folds the same
        // blocks the parser reads as documentation.
        if trimmed.starts_with(cagara_syntax::docs::ITEM_MARKER) {
            run = match run {
                Some((first, _)) => Some((first, i)),
                None => Some((i, i)),
            };
        } else if let Some((first, last)) = run.take() {
            if last > first {
                out.push(FoldingRange {
                    start_line: first as u32,
                    start_character: None,
                    end_line: last as u32,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Comment),
                    collapsed_text: None,
                });
            }
        }
    }
    if let Some((first, last)) = run {
        if last > first {
            out.push(FoldingRange {
                start_line: first as u32,
                start_character: None,
                end_line: last as u32,
                end_character: None,
                kind: Some(FoldingRangeKind::Comment),
                collapsed_text: None,
            });
        }
    }
    out.sort_by_key(|r| (r.start_line, r.end_line));
    out
}

/// Every place that refers to the name under the cursor, in any open file.
///
/// `highlights` answers the same question for one document, because
/// `textDocument/documentHighlight` is defined that way. A *reference* is not: a
/// definition used from three files has three uses, and an editor's "find all
/// references" that listed only the one it was asked from would be quietly wrong —
/// it does not error, it shows a short list.
///
/// The target is resolved once, from the cursor's own file, and then looked for in
/// every module reachable from the root. Each module's offsets are its own, so each
/// is converted with its own `Positions`; a shared offset across two texts would
/// point at unrelated text and still return a plausible range.
pub fn references(
    ws: &Workspace,
    pos: Position,
    include_declaration: bool,
) -> Vec<(PathBuf, Range)> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let root_occs = occurrences(ws, ws.root);
    let Some(target) = p
        .offset(pos)
        .and_then(|off| occ_at(&root_occs, off))
        .map(|o| o.target.clone())
    else {
        return Vec::new();
    };
    // A lambda parameter is identified by the byte offset where it is bound, and an
    // offset means nothing outside its own text: two files can bind a parameter at
    // the same byte and a cross-file match would report one as a reference to the
    // other. So a local is looked for in the cursor's file only, which is also the
    // only place it can appear.
    let cross_file = matches!(target, Target::Global(_));
    let mut out = Vec::new();
    for (m, reachable) in ws.modules_reachable_from(ws.root).into_iter().enumerate() {
        if !reachable || (!cross_file && m != ws.root) {
            continue;
        }
        let md = &ws.modules[m];
        let mp = Positions::new(&md.text);
        for o in occurrences(ws, m) {
            if o.target != target || (!include_declaration && o.decl) {
                continue;
            }
            out.push((md.path.clone(), root_range(&mp, o.start, o.end)));
        }
    }
    out
}

/// What `textDocument/rename` would change, or why it will not.
///
/// The refusals are part of the answer rather than an empty list, because an empty
/// edit set and "this name cannot be renamed" are different things to an editor: the
/// first says the file is already right, the second says nothing was attempted. A
/// rename that silently did nothing would leave the user believing it worked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rename {
    /// Edits per file. Every entry has at least one edit.
    Edits(Vec<(PathBuf, Vec<TextEdit>)>),
    /// A sentence to show the user. Always says *why*, because the cursor landing
    /// on a column and on a definition look identical from the outside.
    Refused(String),
}

/// The name under the cursor, if it is one this server will rename.
///
/// `textDocument/prepareRename` uses this to decide whether to put the editor into
/// rename mode at all: a column name, a literal or the space between two tokens is
/// not a rename target, and the honest answer is a range or a refusal.
pub fn prepare_rename(ws: &Workspace, pos: Position) -> Result<Range, String> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let occs = occurrences(ws, ws.root);
    let o = p
        .offset(pos)
        .and_then(|off| occ_at(&occs, off))
        .ok_or_else(|| {
            "there is no name here to rename: this is a column, a literal or a \
             punctuation mark, and none of them is bound to anything"
                .to_string()
        })?;
    check_renamable(&o.target)?;
    Ok(root_range(&p, o.start, o.end))
}

/// Why `target` cannot be renamed, or `Ok` when it can.
///
/// The prelude is the one interesting refusal. `sum` is a name the *language*
/// defines, and it lives in a file the user does not have: rewriting every use of it
/// in their program would break those uses, not rename the definition. The message
/// says so, because "no" without a reason reads like a bug.
fn check_renamable(target: &Target) -> Result<(), String> {
    match target {
        // The prelude is module 0 (`Workspace::empty` adds it first), which is the
        // same test `definition` uses to keep prelude definitions out of its answer.
        Target::Global(Binding::Def(0, _) | Binding::Overloads(0, _)) => Err(
            "this name is defined by the prelude, in a file you do not have; \
                 give it a name of your own instead of renaming the language's"
                .to_string(),
        ),
        Target::Global(Binding::Def(..) | Binding::Overloads(..) | Binding::Module(_)) => Ok(()),
        Target::Local(_) => Ok(()),
    }
}

/// Rename the name under the cursor to `new_name`, everywhere it means that name.
///
/// The edit set is `references` with declarations included: a definition's own name
/// is one of its occurrences, so renaming a definition rewrites the declaration and
/// every use together. Doing it any other way — a rename that left the definition
/// alone — would produce two names that do not resolve to each other.
///
/// A local is looked for in its own file only, for the reason `references` gives:
/// the identity of a lambda parameter is a byte offset, and an offset is only
/// meaningful in one text.
pub fn rename(ws: &Workspace, pos: Position, new_name: &str) -> Rename {
    if let Err(why) = valid_new_name(new_name) {
        return Rename::Refused(why);
    }
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let occs = occurrences(ws, ws.root);
    let Some(o) = p.offset(pos).and_then(|off| occ_at(&occs, off)) else {
        return Rename::Refused(
            "there is no name here to rename: this is a column, a literal or a \
             punctuation mark, and none of them is bound to anything"
                .to_string(),
        );
    };
    if let Err(why) = check_renamable(&o.target) {
        return Rename::Refused(why);
    }
    let target = o.target.clone();
    // The name being replaced, for the collision check below.
    let old_name = &text[o.start..o.end];
    if old_name == new_name {
        // Not a refusal, but not an edit either: the file already says what was
        // asked for.
        return Rename::Edits(Vec::new());
    }
    if let Err(why) = not_captured(ws, &target, new_name) {
        return Rename::Refused(why);
    }
    let cross_file = matches!(target, Target::Global(_));
    let mut out: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
    for (m, reachable) in ws.modules_reachable_from(ws.root).into_iter().enumerate() {
        if !reachable || (!cross_file && m != ws.root) {
            continue;
        }
        let md = &ws.modules[m];
        let mp = Positions::new(&md.text);
        let edits: Vec<TextEdit> = occurrences(ws, m)
            .into_iter()
            .filter(|x| x.target == target)
            .map(|x| TextEdit {
                range: root_range(&mp, x.start, x.end),
                new_text: new_name.to_string(),
            })
            .collect();
        if !edits.is_empty() {
            out.push((md.path.clone(), edits));
        }
    }
    Rename::Edits(out)
}

/// `new_name` has to be one name, spelled the way this language spells names.
///
/// The check is the *lexer's*, not a re-implemented character class: a name is valid
/// exactly when lexing it produces one `Ident` whose text is the whole input. That
/// keeps the rule in one place, and it rejects `a b`, `1x`, `a.b` and `""` without
/// this function having an opinion about any of them.
fn valid_new_name(new_name: &str) -> Result<(), String> {
    if new_name.is_empty() {
        return Err("a name cannot be empty".to_string());
    }
    let lexed: Vec<_> = cagara_syntax::lexer::lex(new_name)
        .into_iter()
        .filter(|l| !l.kind.is_trivia())
        .collect();
    match lexed.as_slice() {
        [one] if one.kind == cagara_syntax::lexer::Token::Ident && one.text == new_name => Ok(()),
        _ => Err(format!(
            "`{new_name}` is not a name: a name starts with a letter or `_` and \
             continues with letters, digits and `_`"
        )),
    }
}

/// Would `new_name` mean something else where the edit lands?
///
/// A rename that introduces a second meaning for a name does not fail — it changes
/// what the program does, quietly. The two ways that happens here are a definition
/// whose new name is already taken in the same module, and a lambda parameter whose
/// new name is already a definition or another parameter in scope. Both are refused
/// rather than resolved, because the right answer depends on which the user meant.
fn not_captured(ws: &Workspace, target: &Target, new_name: &str) -> Result<(), String> {
    match target {
        Target::Global(Binding::Def(m, _) | Binding::Overloads(m, _)) => {
            let md = &ws.modules[*m];
            match md.scope.get(new_name) {
                None => Ok(()),
                // A prelude name is in scope here without being in this file, and
                // shadowing one is a different decision from colliding with a
                // definition of the user's own. Saying which is which is the
                // difference between an instruction and a guess.
                Some(Binding::Def(0, _) | Binding::Overloads(0, _)) => Err(format!(
                    "`{new_name}` is the prelude's name for something else; picking it \
                     would shadow the language's definition here"
                )),
                Some(_) => Err(format!(
                    "`{new_name}` already names something in this file; pick a name that \
                     is not in use, or rename that one first"
                )),
            }
        }
        // A parameter may take any name that is not already bound where it is used.
        // The lambda's scope is `[binding offset, scope_end)`, so a *different* target
        // spelled `new_name` inside it would be captured by the new binding and the
        // program would silently mean something else.
        Target::Local(at) => {
            let text = &ws.modules[ws.root].text;
            let occs = occurrences(ws, ws.root);
            let Some(bind) = occs
                .iter()
                .find(|o| o.decl && o.target == Target::Local(*at))
            else {
                return Ok(());
            };
            let captured = occs.iter().any(|o| {
                o.target != Target::Local(*at)
                    && o.start >= bind.start
                    && o.end <= bind.scope_end
                    && &text[o.start..o.end] == new_name
            });
            if captured {
                Err(format!(
                    "`{new_name}` already means something inside this lambda; the \
                     parameter would take over its uses"
                ))
            } else {
                Ok(())
            }
        }
        Target::Global(Binding::Module(_)) => Ok(()),
    }
}

/// Occurrences in the root file of the name under the cursor; binding
/// sites are `WRITE`, uses `READ`.
pub fn highlights(ws: &Workspace, pos: Position) -> Vec<(Range, DocumentHighlightKind)> {
    let text = &ws.modules[ws.root].text;
    let p = Positions::new(text);
    let occs = occurrences(ws, ws.root);
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

/// Every definition in every reachable file, for `workspace/symbol`.
///
/// `symbols` answers for one document, because `textDocument/documentSymbol` is defined
/// that way. A *workspace* symbol is the other question — "where is `revenue` defined,
/// in any file" — and answering it with the root's definitions would show a short list
/// without saying so, the same failure `references` had.
///
/// The prelude is skipped: its definitions are the language's, they live in a file the
/// user does not have, and a search for `sum` should not send them to `<prelude>:504`.
/// The path is what says so, which is the same test `definition` uses.
pub fn workspace_symbols(ws: &Workspace) -> Vec<(PathBuf, Range, String, SymbolKind)> {
    let tc = check_reachable(ws);
    let mut out = Vec::new();
    for (m, reachable) in ws.modules_reachable_from(ws.root).into_iter().enumerate() {
        if !reachable {
            continue;
        }
        let md = &ws.modules[m];
        if md.path.to_string_lossy().starts_with('<') {
            // `<prelude>` and `<input>`: no file to open. A location naming a path the
            // editor cannot open is worse than no result.
            continue;
        }
        let p = Positions::new(&md.text);
        for (i, d) in md.module.defs.iter().enumerate() {
            let Some((s, e)) = trimmed(&md.text, d.span) else {
                continue;
            };
            let name_end = (s + d.name.len()).min(e);
            let ty = tc.type_of(m, i).map(str::to_string);
            let kind = if ty.as_deref().is_some_and(|t| t.contains("->")) {
                SymbolKind::FUNCTION
            } else {
                SymbolKind::VARIABLE
            };
            out.push((
                md.path.clone(),
                root_range(&p, s, name_end),
                d.name.clone(),
                kind,
            ));
        }
    }
    // A stable order, so two runs of the same tree answer the same way — the editor
    // keeps the list between keystrokes and a reshuffle is a flicker.
    out.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.start.line.cmp(&b.1.start.line))
    });
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
    let mut locals: Vec<CompletionItem> = occurrences(ws, ws.root)
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
                       bad = users & select { x = \"é\" ++ .age }\n\
                       twice = x => x + x\n\
                       shadow = users => users & where adult\n";

    fn ws() -> Workspace {
        Workspace::open_with(Path::new("/nonexistent/main.cagara"), SRC.to_string())
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    /// Every definition that does not write its type gets one as a hint, and a
    /// definition that does is left alone.
    #[test]
    fn an_unannotated_definition_gets_a_type_hint() {
        let hints = type_hints(&ws());
        let labels: Vec<&str> = hints.iter().map(|(_, l)| l.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.contains("query { s = maybe")),
            "no hint for `q`: {labels:?}"
        );
        // `users` and `twice` write no type either; `bad` writes none and its
        // type is the error's, so it may be open.
        assert!(
            labels.iter().any(|l| l.contains("->")),
            "no hint for a function: {labels:?}"
        );
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

    /// The code is the diagnostic's identity while the message is its wording:
    /// a client may filter or link on it, and rewording a message must not
    /// change it.
    /// The code a client sees is the diagnostic's **kind**, and every kind
    /// maps to a stable slug.
    ///
    /// The set below is exactly the mapping the server published while the code
    /// was read off the message text, so a client that filters on these strings
    /// keeps working. It is asserted on the kind → code direction, which is the
    /// one that matters: the message is wording and may be reworded, and the
    /// code must not move when it is.
    #[test]
    fn every_diagnostic_kind_has_a_stable_code() {
        for (kind, code) in [
            (DiagKind::UnknownColumn, "unknown-column"),
            (DiagKind::UnknownName, "unknown-name"),
            (DiagKind::TypeMismatch, "type-mismatch"),
            (DiagKind::Overload, "overload"),
            (DiagKind::NotACondition, "not-a-condition"),
            (DiagKind::AggregatePlacement, "aggregate-placement"),
            (DiagKind::JoinOverlap, "join-overlap"),
            (DiagKind::JoinSide, "join-side"),
            (DiagKind::Syntax, "syntax"),
            (DiagKind::Recursion, "recursion"),
            (DiagKind::Lowering, "lowering"),
            (DiagKind::Other, "error"),
        ] {
            assert_eq!(diagnostic_code(kind), code, "{kind:?}");
            // The slug is the kind's own, so the two cannot drift apart.
            assert_eq!(kind.code(), code, "{kind:?}");
        }
    }

    /// Each kind is reachable from a real program, and reaches the editor with
    /// its own code.
    ///
    /// The mapping above is only half the contract: a kind that no producer ever
    /// sets is a code no client will ever see, and the classification being
    /// right in the enum but wrong at the construction site is the bug this
    /// change exists to prevent. So each case below is a program, the diagnostic
    /// the editor publishes for it, and the code it must carry.
    #[test]
    fn a_real_diagnostic_of_each_kind_reaches_the_editor_with_its_code() {
        // (source, the code the published diagnostic must carry)
        for (src, code) in [
            // A column the row does not have.
            (
                "t : query { id = int } = table \"m\" \"t\"\n\
                 q = t & select { x = .nope }\n",
                "unknown-column",
            ),
            // A name the language does not have.
            ("q = nope\n", "unknown-name"),
            // Two types that do not unify.
            ("q : int = \"s\"\n", "type-mismatch"),
            // An overload set with no fitting candidate: `+` has no candidate
            // that takes a string and an int. (`sum` in a `select` used to be the
            // example here; it is classified as `aggregate-placement` now, which
            // is the more precise answer, so it moved to the case below.)
            ("q = \"a\" + 1\n", "overload"),
            // An aggregate where a row expression is required.
            (
                "t : query { id = int } = table \"m\" \"t\"\n\
                 q = t & select { x = sum .id }\n",
                "aggregate-placement",
            ),
            // An aggregate where a row expression is required. `count` rather than
            // `sum .id`, because `sum` returns a `maybe` and comparing it to `1`
            // reports the nullability mismatch first — also a real answer, but not the
            // one this case is about.
            (
                "t : query { id = int } = table \"m\" \"t\"\n\
                 q = t & where (count > 1)\n",
                "aggregate-placement",
            ),
            // A join-side column outside a join predicate.
            (
                "a : query { id = int } = table \"m\" \"a\"\n\
                 q = a & select { x = .<id }\n",
                "join-side",
            ),
            // A token the grammar does not accept. (`1 &? 2` is a well-formed pipeline
            // over an `int`, so it is a *type* error, not a syntax one — the parser
            // accepts it and the checker is right to refuse it.)
            ("q = @\n", "syntax"),
            // A definition that refers to itself.
            ("q = q\n", "recursion"),
        ] {
            let ws = Workspace::from_source(src);
            let ds = diagnostics(&ws);
            assert!(
                ds.iter().any(|d| diagnostic_code(d.kind) == code),
                "expected a `{code}` diagnostic for {src:?}, got {:?}",
                ds.iter()
                    .map(|d| (diagnostic_code(d.kind), &d.message))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A file the *lowering* refuses is reported in the editor, at the stage.
    ///
    /// The checker accepts this program — every stage is well typed — and `cagara-sql`
    /// refuses it, because a `distinct` after an `order` on an expression the
    /// projection dropped cannot be written. `cagara --only` reported that and the
    /// editor reported nothing, so a file that would not compile looked clean in the
    /// one place the author was looking.
    #[test]
    fn a_lowering_refusal_reaches_the_editor() {
        let src = "t : query { id = int, name = string } = table \"m\" \"t\"\n\
                   q = t & order [asc (.id + 1)] & select {.id} & distinct\n";
        let ws = Workspace::from_source(src);
        let ds = diagnostics(&ws);
        assert_eq!(ds.len(), 1, "{ds:?}");
        let Report {
            range: r,
            message: msg,
            severity,
            kind,
        } = &ds[0];
        assert_eq!(*severity, Severity::Error);
        // A lowering refusal is its own kind: the program is well typed and the
        // target cannot express it, which is not a type error.
        assert_eq!(*kind, DiagKind::Lowering, "{msg}");
        assert!(msg.contains("cannot follow an `order`"), "{msg}");
        // The refusal is on line 1 and points at the stage, not at the file's start.
        assert_eq!(r.start.line, 1, "{r:?}");
        assert!(r.start.character > 0, "not column zero: {r:?}");
    }

    /// With no dialect configured, a refusal that is about a *target* is not
    /// reported.
    ///
    /// This is the negative control for the ANSI fallback. A `groups` frame is refused
    /// on MySQL and T-SQL and fine on ANSI; painting it red in a file whose target the
    /// server was never told about would be a true statement about somebody else's
    /// choice. So with no `cagara.dialect` the editor reports what is true of every
    /// target, and [`a_configured_dialect_reaches_the_lowering`] is the positive half:
    /// once the target *is* known, the refusal appears.
    #[test]
    fn a_target_specific_refusal_is_not_reported_without_a_configured_dialect() {
        let src = "f : query { k = int, n = int } = table \"m\" \"f\"\n\
                   q = f & select { s = over { order = [asc .k], \
                   frame = groups (preceding 1) currentRow } (sum .n) }\n";
        let ws = Workspace::from_source(src);
        let ds = diagnostics(&ws);
        assert!(
            !ds.iter()
                .any(|d| d.message.contains("`groups` window frame")),
            "a `groups` frame is fine on ANSI, which is the fallback: {ds:?}"
        );
        // And it *is* refused when a target that lacks the clause is named, so the
        // silence above is about the dialect and not about the check being missing.
        let rels = cagara_hir::root_queries(&ws);
        let (_, r) = rels.iter().find(|(n, _)| n == "q").expect("q");
        let rel = r.as_ref().expect("q type-checks");
        for dialect in [cagara_sql::Dialect::Mysql, cagara_sql::Dialect::Tsql] {
            assert!(
                cagara_sql::compile(
                    rel,
                    cagara_sql::Options {
                        dialect,
                        ..Default::default()
                    }
                )
                .is_err(),
                "{dialect:?} refuses it"
            );
        }
    }

    /// The configured dialect reaches the lowering, so the editor and the compiler
    /// agree about a target-specific refusal.
    ///
    /// This is the point of the setting: a file written for MySQL should show the
    /// `groups` squiggle the compiler will report, and the same file written for
    /// DuckDB should not. The diagnostic is the observable, so the test asks for it
    /// under both targets rather than trusting that the value was threaded.
    #[test]
    fn a_configured_dialect_reaches_the_lowering() {
        let src = "f : query { k = int, n = int } = table \"m\" \"f\"\n\
                   q = f & select { s = over { order = [asc .k], \
                   frame = groups (preceding 1) currentRow } (sum .n) }\n";
        let ws = Workspace::from_source(src);
        let refused = |d: cagara_sql::Dialect| {
            diagnostics_with(&ws, d)
                .into_iter()
                .any(|r| r.message.contains("`groups` window frame"))
        };
        // MySQL and T-SQL document `{ ROWS | RANGE }`, so the frame is refused and the
        // editor says so.
        assert!(refused(cagara_sql::Dialect::Mysql), "mysql");
        assert!(refused(cagara_sql::Dialect::Tsql), "tsql");
        // DuckDB and PostgreSQL read `GROUPS`, so the same file is clean.
        assert!(!refused(cagara_sql::Dialect::DuckDb), "duckdb");
        assert!(!refused(cagara_sql::Dialect::Postgres), "postgres");
        // And the fallback is ANSI, which has every frame unit: no diagnostic.
        assert!(!refused(cagara_sql::Dialect::Ansi), "ansi");
    }

    #[test]
    fn diagnostics_have_utf16_ranges() {
        let ds = diagnostics(&ws());
        assert_eq!(ds.len(), 1, "{ds:?}");
        let Report {
            range: r,
            message: msg,
            severity,
            ..
        } = &ds[0];
        assert_eq!(*severity, Severity::Error);
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
        assert!(h.contains("adult : expr { age = ?a | ?b } bool"), "{h}");
        // `sum` is an overload set in the prelude.
        let h = one_line(hover(&ws, at("sum .age", 1)).unwrap());
        let (here, general) = h.split_once("defined as").unwrap();
        assert!(
            here.contains("sum : expr { age = int, id = int, name = string } int -> "),
            "{h}"
        );
        // Every signature of the set is listed: the numeric pair, the nullable
        // pair (which is why `sum` over a `maybe` column needs no `sumMaybe`),
        // and the decimal pair, whose width is stated as the portable 38.
        assert_eq!(general.matches("sum : ").count(), 6, "{h}");
        assert!(general.contains("(decimal 38 ?s)"), "{h}");
        assert!(general.contains("(maybe int) -> agg"), "{h}");
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
    const COLUMNS: &str = "--- The user table.\n\
                           users : query {\n\
                           \x20   --- The primary key.\n\
                           \x20   id = int,\n\
                           \x20   --- Age in years.\n\
                           \x20   age = int\n\
                           \x20 } = table \"p\" \"users\"\n\
                           \n\
                           --- The order table.\n\
                           orders : query {\n\
                           \x20   --- The amount charged.\n\
                           \x20   amount = float,\n\
                           \x20   --- The user who placed it.\n\
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
        let src = "users : query {\n    --- Key.\n    id = int,\n    name = string\n  } = table \"p\" \"t\"\n\n\
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
        let src =
            "--- T.\nusers : query {\n    --- Age.\n    age = int\n  } = table \"p\" \"t\"\n\n\
                   alias = users\n\
                   via_alias = alias & where (.age > 0)\n\
                   shorthand = users & where (.age > 0)\n\
                   sorted = users & order [asc .age]\n\
                   unioned = union users users & where (.age > 0)\n";
        let ws = Workspace::open_with(Path::new("/nonexistent/alias.cagara"), src.to_string());
        for needle in [
            "via_alias = alias & where (.age > 0)",
            "shorthand = users & where (.age > 0)",
            "sorted = users & order [asc .age]",
            "unioned = union users users & where (.age > 0)",
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
        assert!(h.contains(".a : expr ?r int"), "{h}");
    }

    /// A column that `prefix` renamed is no longer the declared one: the doc
    /// would be about a name that the record does not have.
    #[test]
    fn a_renamed_column_has_no_documentation() {
        let src =
            "--- T.\nusers : query {\n    --- Key.\n    id = int\n  } = table \"p\" \"t\"\n\n\
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
            general.contains("select : expr ?r (row ?s) -> query ?r -> query ?s"),
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
        // is `expr ?r float`, not the bare value type.
        let h = at("where (.score", 8);
        assert!(h.contains(".score : expr ?r float"), "{h}");
        // `18` lifted to the column's type.
        let h = at(">= 18", 3);
        assert!(h.contains("18 : float"), "{h}");
        let h = at("&& .name", 3);
        assert!(h.contains(".name : expr ?r string"), "{h}");
        let h = at("== \"a\"", 3);
        assert!(h.contains("\"a\" : string"), "{h}");
        // A literal in a record keeps its own type.
        let h = at("k = 1", 4);
        assert!(h.contains("1 : int"), "{h}");
    }

    /// A column hovers as the expression type it has, not as its value type.
    ///
    /// A column is not a bare value: it is row-phase, which is what every
    /// `expr ?r a` parameter accepts, and writing `string` for it hides both the
    /// phase and the row — a reader copying the hover into a helper's signature
    /// wrote `string -> string`, which the checker then refused. `r` is the row
    /// variable a signature would name, so the hover is the shape the reader has
    /// to write; the concrete row is deliberately elided, because printing it
    /// repeats the whole table for every column and puts the column first,
    /// which is not the row's order.
    ///
    /// A literal is *not* wrapped: it may be a label rather than an expression
    /// (`limit 2`, `prefix "u_"`), where `expr ?r …` would be wrong.
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

        assert_eq!(at("upper .name", 8), ".name : expr ?r string");
        // A compound value type is parenthesised, as the printer spells it, so
        // the hover parses as the type it means.
        assert_eq!(
            at("coalesce 0.0 .score", 15),
            ".score : expr ?r (maybe float)"
        );
        // A side-qualified column is an expression over the joined row.
        assert_eq!(at(".<id + 1", 2), ".<id : expr (join ?r ?s) int");
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
        assert!(h.contains(".score : expr ?r float"), "{h}");
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
                   twice = (x : expr ?r int) => x + x\n\
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
            // `expr ?r int` is one character longer than the old `expr r int`,
            // so the two uses after the annotation sit one column further on.
            vec![range(1, 9, 10), range(1, 29, 30), range(1, 33, 34)]
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

    /// A lambda parameter is not matched across files.
    ///
    /// `workspace_symbols` covers every reachable file, and leaves the prelude out.
    ///
    /// `symbols` answers for one document — that is what an outline is. A workspace
    /// search that returned only the root's definitions would show a short list
    /// without saying so, which is the failure `references` had.
    #[test]
    fn workspace_symbols_cover_every_open_file() {
        let dir = std::env::temp_dir().join(format!("cagara-lsp-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.cagara"), "helper = 1\nother = 2\n").unwrap();
        let main = dir.join("main.cagara");
        let src = "import \"lib.cagara\" as lib\nq = lib.helper\n";
        let ws = Workspace::open_with(&main, src.to_string());
        let found = workspace_symbols(&ws);
        let names: Vec<&str> = found.iter().map(|(_, _, n, _)| n.as_str()).collect();
        // The root's own definition and both of the library's.
        assert!(names.contains(&"q"), "{names:?}");
        assert!(names.contains(&"helper"), "the imported file: {names:?}");
        assert!(names.contains(&"other"), "the imported file: {names:?}");
        // The prelude is a file the user does not have, so it is not a jump target.
        assert!(
            !names.contains(&"sum"),
            "the prelude is not searchable: {names:?}"
        );
        // Every result names a real path, and the imported ones name the library.
        let paths: Vec<String> = found
            .iter()
            .map(|(p, _, _, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(paths.contains(&"lib.cagara".to_string()), "{paths:?}");
        // Sorted by name, so two runs answer the same way.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "stable order: {names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A rename of an imported definition reaches into the file that defines it.
    ///
    /// This is the case a single-file rename gets wrong without anyone noticing: the
    /// edit set looks complete — it renames the use — and the definition in the other
    /// file still has the old name, so the program stops resolving. The edit set has
    /// to carry both files.
    #[test]
    fn rename_reaches_across_an_import() {
        let dir = std::env::temp_dir().join(format!("cagara-lsp-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.cagara"), "helper = 1\n").unwrap();
        let main = dir.join("main.cagara");
        let src = "import \"lib.cagara\" as lib\nq = lib.helper\n";
        let ws = Workspace::open_with(&main, src.to_string());
        let Rename::Edits(files) = rename(&ws, at_in(src, "lib.helper", 4), "aid") else {
            panic!("renaming an imported definition is allowed");
        };
        let mut names: Vec<String> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["lib.cagara".to_string(), "main.cagara".to_string()],
            "both files are edited: {files:?}"
        );
        // One edit in each: the definition in `lib`, the use in `main`.
        for (path, edits) in &files {
            assert_eq!(edits.len(), 1, "{}: {edits:?}", path.display());
            assert_eq!(edits[0].new_text, "aid");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `Target::Local` is the byte offset where the parameter is bound, and an
    /// offset is only meaningful inside one text. Two files can bind a parameter at
    /// the same byte — this test builds them so they do — and without a guard the
    /// one would be reported as a reference to the other. The guard is that a local
    /// is looked for in the cursor's file only, which is also the only place it can
    /// appear.
    #[test]
    fn a_lambda_parameter_is_not_matched_across_files() {
        let dir = std::env::temp_dir().join(format!("cagara-lsp-locals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = "import \"lib.cagara\" as lib\nq = lib.f\nr = x => x\n";
        // The lambda's `x` binds at byte 41 in the root. `lib.cagara` is padded so
        // its own lambda binds at byte 41 too — the collision is the point.
        let at = src.find("r = x").expect("the lambda") + 4;
        // `f = x => x` puts its `x` four bytes into the line, so the padding before
        // it is `at - 4` bytes of harmless definitions.
        let pad = "p = 0\n".repeat((at - 4) / 6);
        let lib = format!("{pad}{}f = x => x\n", "\n".repeat((at - 4) % 6));
        assert_eq!(
            lib.find('x'),
            Some(at),
            "the two offsets must collide: {lib:?}"
        );
        std::fs::write(dir.join("lib.cagara"), &lib).unwrap();
        let main = dir.join("main.cagara");
        let ws = Workspace::open_with(&main, src.to_string());
        let rs = references(&ws, at_in(src, "r = x", 4), true);
        let files: Vec<String> = rs
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.iter().all(|f| f == "main.cagara"),
            "a local belongs to one file: {files:?}"
        );
        assert_eq!(rs.len(), 2, "the parameter and its use: {rs:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A rename rewrites the definition and every use, and nothing else.
    ///
    /// The trap is the same one `references` has: `users` is both a definition and a
    /// lambda parameter in `shadow`, and renaming the definition must leave the
    /// parameter alone — they are different names that happen to be spelled the same.
    #[test]
    fn rename_rewrites_the_definition_and_its_uses() {
        let ws = ws();
        let Rename::Edits(files) = rename(&ws, at("q = users", 4), "people") else {
            panic!("renaming a definition is allowed");
        };
        assert_eq!(files.len(), 1, "one file, one entry: {files:?}");
        let (_, edits) = &files[0];
        // `users` on lines 0, 2 and 5 is the definition and its two uses; the
        // parameter in `shadow` (line 5, column 9) is a different name.
        assert_eq!(
            edits.iter().map(|e| e.range).collect::<Vec<_>>(),
            vec![range(0, 0, 5), range(2, 4, 9), range(3, 6, 11)],
            "{edits:?}"
        );
        assert!(edits.iter().all(|e| e.new_text == "people"), "{edits:?}");
    }

    /// A lambda parameter is renamed within its scope, not across the file.
    #[test]
    fn rename_handles_a_lambda_parameter() {
        let ws = ws();
        let Rename::Edits(files) = rename(&ws, at("=> x", 3), "n") else {
            panic!("renaming a parameter is allowed");
        };
        let (_, edits) = &files[0];
        assert_eq!(
            edits.iter().map(|e| e.range).collect::<Vec<_>>(),
            vec![range(4, 8, 9), range(4, 13, 14), range(4, 17, 18)],
            "the parameter and both uses: {edits:?}"
        );
    }

    /// The refusals say *why*, and each reason is a different one.
    ///
    /// A rename that silently did nothing is the failure mode this is guarding: an
    /// editor shows "no changes" and the user believes the rename happened. So the
    /// three ways the gesture can be wrong — the prelude's name, a column, and a name
    /// already in use — are three messages, not three empty edit sets.
    #[test]
    fn a_rename_that_would_be_wrong_is_refused_with_a_reason() {
        let ws = ws();
        // The prelude defines `sum`; the user cannot rename the language's names.
        match rename(&ws, at("sum .age", 1), "total") {
            Rename::Refused(why) => assert!(why.contains("prelude"), "{why}"),
            other => panic!("a prelude name is refused: {other:?}"),
        }
        // `.age` is a column, not a name with a binding.
        match rename(&ws, at(".age >= 18", 1), "years") {
            Rename::Refused(why) => assert!(why.contains("no name here"), "{why}"),
            other => panic!("a column is refused: {other:?}"),
        }
        // `adult` cannot become `users`: that name is taken in this file.
        match rename(&ws, at("where adult", 7), "users") {
            Rename::Refused(why) => assert!(why.contains("already names"), "{why}"),
            other => panic!("a collision is refused: {other:?}"),
        }
        // And the new name has to be a name.
        match rename(&ws, at("where adult", 7), "two words") {
            Rename::Refused(why) => assert!(why.contains("is not a name"), "{why}"),
            other => panic!("a non-name is refused: {other:?}"),
        }
        // Renaming to what it already says is not an error, and not an edit either.
        assert_eq!(
            rename(&ws, at("where adult", 7), "adult"),
            Rename::Edits(Vec::new())
        );
    }

    /// `signature_help` reports the call the cursor is *in*, and which argument.
    ///
    /// The trap is nesting: `q = users & select { v = toDecimal 10 2 1.5 }` has `_&_`
    /// as an application whose second argument span contains everything after it, so
    /// taking the outermost match reports `_&_`'s signature for a cursor sitting inside
    /// `toDecimal`. The innermost call is what the reader is typing.
    #[test]
    fn signature_help_finds_the_innermost_call() {
        let src = "t : query { id = int } = table \"m\" \"t\"\n\
                   q = t & select { v = toDecimal 10 2 1.5 }\n";
        let ws = Workspace::from_source(src);
        // The three argument positions of `toDecimal`, and the call itself.
        let base = src.find("toDecimal").expect("toDecimal");
        let at_byte = |off: usize| {
            let before = &src[..off];
            let line = before.matches('\n').count() as u32;
            let character = before.rsplit('\n').next().unwrap().chars().count() as u32;
            Position { line, character }
        };
        for (want, off) in [(0usize, base + 11), (1, base + 14), (2, base + 16)] {
            let (sigs, active) =
                signature_help(&ws, at_byte(off)).unwrap_or_else(|| panic!("no help at {off}"));
            assert!(
                sigs[0].starts_with("toDecimal"),
                "the innermost call, not `&`: {sigs:?}"
            );
            assert_eq!(active, want, "argument {want} at offset {off}");
        }
        // Outside any call's arguments, there is nothing to say.
        assert!(signature_help(&ws, at_byte(0)).is_none());
    }

    /// `prepareRename` answers with the range the editor should select.
    #[test]
    fn prepare_rename_gives_the_range_or_refuses() {
        let ws = ws();
        assert_eq!(
            prepare_rename(&ws, at("where adult", 7)),
            Ok(range(2, 18, 23)),
            "the `adult` token"
        );
        assert!(prepare_rename(&ws, at(".age >= 18", 1)).is_err());
        assert!(prepare_rename(&ws, at("sum .age", 1)).is_err());
    }

    /// Folding: the multi-line definitions, the `##` blocks, and nothing else.
    ///
    /// A one-line definition gets no range, because an editor given a range whose
    /// start and end are the same line folds a line into itself. In `SRC` that is
    /// `adult`, `q`, `bad`, `twice` and `shadow` — every definition is one line — so
    /// the only ranges are the doc blocks of the source below.
    /// Semantic tokens classify names by what they *are*, not by how they look.
    ///
    /// `users` appears three times in `SRC` and means the same thing each time, so it
    /// is coloured the same. `x` in `twice` is a parameter, `age` in `.age` is a column
    /// (a `property`), and the point of the whole feature is that last one: nothing
    /// lexical can tell a column from a definition.
    #[test]
    fn semantic_tokens_classify_names() {
        let ws = ws();
        let tokens = semantic_tokens(&ws);
        let kind_at = |line: u32, start: u32| {
            tokens
                .iter()
                .find(|t| t.line == line && t.start == start)
                .map(|t| t.kind)
                .unwrap_or_else(|| panic!("no token at {line}:{start}: {tokens:?}"))
        };
        // `users` on line 0 is a definition whose type is a query, so a variable;
        // `adult` on line 1 is a predicate, also a variable.
        assert_eq!(kind_at(0, 0), TY_VARIABLE, "`users`: {tokens:?}");
        assert_eq!(kind_at(1, 0), TY_VARIABLE, "`adult`: {tokens:?}");
        // `x` in `twice` is a lambda parameter.
        assert_eq!(kind_at(4, 8), TY_PARAMETER, "`x`: {tokens:?}");
        // `sum` is a prelude function, applied to an argument.
        assert_eq!(kind_at(2, 36), TY_FUNCTION, "`sum`: {tokens:?}");
    }

    /// Delta encoding is relative to the previous token on the same line only.
    ///
    /// This is the classic semantic-token bug: use an absolute start everywhere and the
    /// first line looks right while every later line is progressively further off. The
    /// assertion is on the encoded numbers, because that is what the editor reads.
    #[test]
    fn semantic_token_deltas_reset_on_a_new_line() {
        let tokens = vec![
            Token {
                line: 0,
                start: 2,
                len: 1,
                kind: 0,
            },
            Token {
                line: 0,
                start: 9,
                len: 3,
                kind: 1,
            },
            Token {
                line: 2,
                start: 4,
                len: 2,
                kind: 2,
            },
        ];
        assert_eq!(
            encode_tokens(&tokens),
            vec![[0, 2, 1, 0, 0], [0, 7, 3, 1, 0], [2, 4, 2, 2, 0]],
            "the third token's start is absolute because the line changed"
        );
    }

    #[test]
    fn folding_covers_definitions_and_doc_blocks() {
        let src = "--- A file about users.\n\
                   --- It spans two lines.\n\
                   users : query { id = int, name = string } = table \"p\" \"users\"\n\
                   q = users & select {\n\
                   \x20 id = .id,\n\
                   \x20 name = .name,\n\
                   }\n\
                   r = users\n";
        let ws = Workspace::from_source(src);
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        let fs = folding_ranges(&ws);
        // The doc block on lines 0..1, as a comment range.
        assert!(
            fs.iter().any(|f| f.start_line == 0
                && f.end_line == 1
                && f.kind == Some(FoldingRangeKind::Comment)),
            "the `##` block: {fs:?}"
        );
        // `q` spans lines 3..6. Its range starts at the name, so `q` stays visible.
        let q = fs
            .iter()
            .find(|f| f.start_line == 3)
            .unwrap_or_else(|| panic!("no fold for `q`: {fs:?}"));
        assert_eq!(q.end_line, 6, "{q:?}");
        assert_eq!(q.kind, None, "a definition is not a comment: {q:?}");
        // `users` and `r` are one line each, so they get no range.
        assert!(
            !fs.iter().any(|f| f.start_line == 2 || f.start_line == 7),
            "one-line definitions are not foldable: {fs:?}"
        );
    }

    /// A `catalog` block folds as one unit, even though it lowers to many definitions.
    ///
    /// This is the case the AST cannot answer: `catalog "s" { a = {…} b = {…} }` becomes
    /// one table definition per entry, so the block no longer exists after lowering. The
    /// parse tree still has it.
    #[test]
    fn folding_covers_a_catalog_block() {
        let src = "catalog \"s\" {\n\
                   \x20 users = { id = int },\n\
                   \x20 orders = { id = int },\n\
                   }\n";
        let ws = Workspace::from_source(src);
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        let fs = folding_ranges(&ws);
        assert!(
            fs.iter().any(|f| f.start_line == 0 && f.end_line == 3),
            "the whole catalog block: {fs:?}"
        );
    }

    #[test]
    fn references_respect_shadowing() {
        let ws = ws();
        // `users` from its use in `q`: the definition and two uses, not the
        // lambda parameter in `shadow`.
        let ranges = |rs: Vec<(PathBuf, Range)>| rs.into_iter().map(|(_, r)| r).collect::<Vec<_>>();
        let rs = references(&ws, at("q = users", 4), true);
        assert_eq!(
            ranges(rs),
            vec![range(0, 0, 5), range(2, 4, 9), range(3, 6, 11)]
        );
        let rs = references(&ws, at("q = users", 4), false);
        assert_eq!(ranges(rs), vec![range(2, 4, 9), range(3, 6, 11)]);
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
                   b = users & innerJoin orders (.<id == .>u)\n\
                   c = .age >= 18 && .\n";
        // A partial name inside a stage: the table's columns, in order.
        assert_eq!(fields(src, pos(2, 22)), ["id", "name", "age"]);
        // Join sides: `.<` reads the left input, `.>` the right one.
        assert_eq!(fields(src, pos(3, 33)), ["id", "name", "age"]);
        assert_eq!(fields(src, pos(3, 41)), ["oid", "user_id"]);
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
    /// row ?a `case` sees is built in the order its arms first mention columns
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
                    & select {id = .lit_id, id44 = .lit_label ++ .lit_label ++ .}\n";
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

#[cfg(test)]
mod severity_tests {
    use super::diagnostics;
    use cagara_hir::workspace::Workspace;

    /// A shared join column publishes nothing at all.
    ///
    /// It used to be the one *warning* the language had. The right column is
    /// renamed to `right_<name>` instead of being dropped, so there is nothing to
    /// report — which is the stronger claim, and the one worth pinning: a
    /// well-formed join produces no diagnostic, so an editor shows no squiggle on
    /// it.
    #[test]
    fn a_shared_join_column_publishes_nothing() {
        let src = "users : query { id = int, name = string } = table \"s\" \"u\"\n\
                   orders : query { id = int, user_id = int } = table \"s\" \"o\"\n\
                   q = users & innerJoin orders (.<id == .>user_id)\n";
        let ws = Workspace::from_source(src);
        assert!(ws.diags.is_empty(), "{:?}", ws.diags);
        let ds = diagnostics(&ws);
        assert!(ds.is_empty(), "{ds:?}");
    }
}

#[cfg(test)]
mod nullable_fix_tests {
    use super::code_actions;
    use cagara_hir::workspace::Workspace;
    use lsp_types::{Position, Range};

    /// The fixes offered for the diagnostic that covers this position.
    fn titles_at(src: &str, line: u32, character: u32) -> Vec<String> {
        let ws = Workspace::from_source(src);
        let p = Position { line, character };
        code_actions(&ws, Range { start: p, end: p })
            .into_iter()
            .map(|f| f.title)
            .collect()
    }

    /// A `maybe` handed to a position that wants a present value is wrapped in
    /// `coalesce`, with a default for the type; the other direction is `just`.
    ///
    /// The suggestion cannot come from the checker — the refusal is reported by
    /// constraint resolution, where the span of the value is gone — so the rule
    /// reads the message the way the sort-key and rename rules do. Both wrap the
    /// *column*, not the field it sits in: `upper .s` becomes
    /// `upper (.s ?? "")`.
    #[test]
    fn a_nullable_mismatch_offers_its_wrapper() {
        let src = "t : query { s = maybe string, r = string } = table \"s\" \"t\"\n\
                   q = t & select { x = upper .s }\n";
        let titles = titles_at(src, 1, 9);
        assert!(titles.iter().any(|t| t.contains("??")), "{titles:?}");

        let src = "t : query { r = string } = table \"s\" \"t\"\n\
                   q = t & select { y = coalesce \"d\" .r }\n";
        let titles = titles_at(src, 1, 9);
        assert!(titles.iter().any(|t| t.contains("just")), "{titles:?}");
    }

    /// The edit wraps the column, and it type-checks once applied.
    #[test]
    fn the_wrapper_wraps_the_column() {
        let src = "t : query { s = maybe string } = table \"s\" \"t\"\n\
                   q = t & select { x = upper .s }\n";
        let ws = Workspace::from_source(src);
        let p = Position {
            line: 1,
            character: 9,
        };
        let fixes = code_actions(&ws, Range { start: p, end: p });
        let fix = fixes
            .iter()
            .find(|f| f.title.contains("??"))
            .unwrap_or_else(|| {
                panic!(
                    "no coalesce fix: {:?}",
                    fixes.iter().map(|f| &f.title).collect::<Vec<_>>()
                )
            });
        // The checker's own suggestion: two insertions around the column, so
        // applying them leaves `.s` between `(` and ` ?? "")`.
        let (open, close) = (&fix.edits[0], &fix.edits[1]);
        assert_eq!(open.new_text, "(");
        assert_eq!(close.new_text, " ?? \"\")");
        // `.s`, the column — not the field `x = upper .s` around it.
        let line = src.lines().nth(1).unwrap();
        assert_eq!(line[27..29].to_string(), ".s");
        assert_eq!(open.range.start.character, 27);
        assert_eq!(close.range.start.character, 29);
    }
}

// ── code actions and inlay hints ─────────────────────────────

/// One fix the language server can offer for a diagnostic at the cursor.
pub struct Fix {
    pub title: String,
    /// The edits to apply together. A rename touches two places — the operand
    /// and every reference to the renamed column in the predicate — so a fix is
    /// a list, not one edit.
    pub edits: Vec<TextEdit>,
}

/// Text edits for the diagnostics that overlap `range`.
///
/// Two sources, in this order:
///
/// 1. The **suggestion the diagnostic carries** (`Diag::suggestions`). The
///    checker knows the span and the rewrite — a join's shared column can be
///    renamed in the operand and in the predicate, and a nullable mismatch wraps
///    the column it refused — so the edit is data, not a guess.
/// 2. The rules that are read off the message, for the diagnostics that carry
///    no suggestion: a sort key that does not say its direction, a `maybe` used
///    where a present value is wanted (wrap it in `coalesce`), and a name a
///    "did you mean" already resolved.
///
/// A `maybe` mismatch is in *both*: the checker keeps the span of every column
/// access, so the projection's refusal can name the column it refused and the
/// suggestion arrives with the diagnostic. The message rule stays as the
/// fallback for a diagnostic that arrives without one.
pub fn code_actions(ws: &Workspace, range: Range) -> Vec<Fix> {
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
    let root = md.path.display().to_string();
    let mut out = Vec::new();
    // What the diagnostic itself knows first: a suggestion is a span and a
    // rewrite the checker decided, so it needs no reading of the message.
    let mut answered: HashSet<(Range, String)> = HashSet::new();
    for d in compile_diagnostics(ws).iter().filter(|d| d.path == root) {
        if d.suggestions.is_empty() {
            continue;
        }
        let r = diag_range(d);
        if !overlaps(&r, &range) {
            continue;
        }
        for s in &d.suggestions {
            let Some(edits) = suggestion_edits(&p, &md.text, s) else {
                continue;
            };
            answered.insert((r, s.title.clone()));
            out.push(Fix {
                title: s.title.clone(),
                edits,
            });
        }
    }
    // Then the rules that are read off the message, for the diagnostics that
    // carry no suggestion.
    for Report {
        range: r, message, ..
    } in diagnostics(ws)
    {
        if answered.iter().any(|(ar, _)| *ar == r) {
            continue;
        }
        if !overlaps(&r, &range) {
            continue;
        }
        let (Some(start), Some(end)) = (p.offset(r.start), p.offset(r.end)) else {
            continue;
        };
        let (Some(text), full) = (md.text.get(start..end), md.text.as_str()) else {
            continue;
        };
        if message.contains("a sort key must say its direction") {
            for (word, title) in [
                ("asc", "Say `asc`: `asc (…)`"),
                ("desc", "Say `desc`: `desc (…)`"),
            ] {
                out.push(Fix {
                    title: title.to_string(),
                    edits: vec![TextEdit {
                        range: r,
                        new_text: format!("{word} ({text})"),
                    }],
                });
            }
        } else if let Some(fix) = nullable_wrap(&message, full, &p, &r) {
            out.push(fix);
        } else if let Some(name) = suggested_name(&message) {
            // The message resolved the typo already; the edit renames the token
            // it means, which is the one closest to that name in the range.
            if let Some((s, e)) = closest_token(text, &name) {
                out.push(Fix {
                    title: format!("Rename to `{name}`"),
                    edits: vec![TextEdit {
                        range: root_range(&p, start + s, start + e),
                        new_text: name,
                    }],
                });
            }
        }
    }
    out
}

/// Do the two ranges touch? Line-only overlap is enough: the client asks about
/// the cursor's selection, and a diagnostic one line away is not it.
fn overlaps(a: &Range, b: &Range) -> bool {
    a.end.line >= b.start.line && b.end.line >= a.start.line
}

/// Turn one suggestion into the edits the client applies.
///
/// `Retoken` is the one that reads the text: it rewrites every occurrence of a
/// token *inside* a span, which is how a join predicate's `.<id` follows the
/// column it was renamed to.
fn suggestion_edits(
    p: &Positions,
    text: &str,
    s: &cagara_hir::workspace::Suggestion,
) -> Option<Vec<TextEdit>> {
    use cagara_hir::workspace::SuggestionEdit;
    let mut edits = Vec::new();
    for e in &s.edits {
        match e {
            SuggestionEdit::Surround {
                span,
                before,
                after,
            } => {
                let r = span_range(p, *span)?;
                edits.push(TextEdit {
                    range: Range {
                        start: r.start,
                        end: r.start,
                    },
                    new_text: before.clone(),
                });
                edits.push(TextEdit {
                    range: Range {
                        start: r.end,
                        end: r.end,
                    },
                    new_text: after.clone(),
                });
            }
            SuggestionEdit::Retoken { span, from, to } => {
                let (start, end) = (span.start as usize, span.end as usize);
                let body = text.get(start..end)?;
                for (i, _) in body.match_indices(from.as_str()) {
                    edits.push(TextEdit {
                        range: span_range(
                            p,
                            Span {
                                start: (start + i) as u32,
                                end: (start + i + from.len()) as u32,
                            },
                        )?,
                        new_text: to.clone(),
                    });
                }
            }
        }
    }
    (!edits.is_empty()).then_some(edits)
}

fn span_range(p: &Positions, span: Span) -> Option<Range> {
    let (start, end) = (span.start as usize, span.end as usize);
    (start <= end).then(|| root_range(p, start, end))
}

/// The name after "did you mean `…`?", if the message has one.
fn suggested_name(message: &str) -> Option<String> {
    let rest = message.split("did you mean `").nth(1)?;
    let name = rest.split('`').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The identifier inside `text` that is closest to `target`, by Levenshtein
/// distance, if any is close enough to be the typo rather than a coincidence.
/// The `coalesce` / `just` fix for a nullable mismatch, read off the message.
///
/// The message names both sides — `expected string, found maybe string` — and the
/// column it happened at (`field \`s\``), so both the direction and the token to
/// rewrite are in the text the diagnostic covers.
///
/// **This is a fallback, and nothing reaches it.** The checker does carry this
/// suggestion now (`check::ty::nullable_fix` builds it from the same message plus
/// the diagnostic's span), so `code_actions` finds it in `d.suggestions` and skips
/// this rule. Measured rather than assumed: instrumenting the guard below and
/// running the whole language-server suite showed the rule called once, with a
/// message that failed the guard, and the body never reached at all. It is kept
/// because it is the only thing that would cover "the checker had a suggestion but
/// the editor could not apply it" (`suggestion_edits` returning `None` skips
/// `answered`), which is a real if unexercised path — but the next reader should
/// know it is unproven rather than assume a test covers it.
///
/// When it *is* reached, the message must still be chained: it takes the last
/// `field \`…\`` as the column to wrap, which is why the checker's message for a
/// value inside a call is `field \`x\`: field \`s\`: …`. A message with a single
/// label names the projection field, and `.x` is not on the line — the column is.
fn nullable_wrap(message: &str, text: &str, p: &Positions<'_>, r: &Range) -> Option<Fix> {
    if !message.contains("only one side is nullable") {
        return None;
    }
    let rest = message.split("expected ").nth(1)?;
    let (expected, found) = rest.split_once(", found ")?;
    // A type can contain spaces (`maybe string`, `list int`): cut at the `;`
    // that introduces the hint, not at the first space.
    let found = found.split(';').next().unwrap_or(found).trim();
    let expected = expected.trim();
    // The innermost field is the last one named: the outer prefix is the field
    // the expression sits in, the inner one is the value that would be wrapped.
    let field = message
        .match_indices("field `")
        .filter_map(|(i, _)| {
            let tail = &message[i + 7..];
            tail.find('`').map(|j| &tail[..j])
        })
        .last()?;
    let default = scalar_default(expected);
    let (before, after, title) = match (expected.starts_with("maybe "), found.starts_with("maybe "))
    {
        (false, true) => {
            let d = default?;
            // The infix fallback, the way it is written by hand — `(.s ?? "")`,
            // parenthesized so the rewrite is safe wherever the value sat.
            (
                "(".to_string(),
                format!(" ?? {d})"),
                format!("Use `?? {d}`"),
            )
        }
        (true, false) => (
            "just (".to_string(),
            ")".to_string(),
            "Wrap in `just`".to_string(),
        ),
        _ => return None,
    };
    // The column can sit anywhere on the line, not only inside the diagnostic's
    // range: the range covers the field the expression is in, and the type
    // mismatch is about the value inside it.
    let line_start = p.offset(Position {
        line: r.start.line,
        character: 0,
    })?;
    let line_end = p.offset(Position {
        line: r.start.line,
        character: u32::MAX,
    })?;
    let line = text.get(line_start..line_end)?;
    let (at, len) = column_span(line, field)?;
    let (from, to) = (line_start + at, line_start + at + len);
    Some(Fix {
        title,
        edits: vec![TextEdit {
            range: Range {
                start: p.position(from),
                end: p.position(to),
            },
            new_text: format!("{before}{}{after}", &text[from..to]),
        }],
    })
}

/// The offset and length of a column reference in `text`, preferring `.name`.
fn column_span(text: &str, name: &str) -> Option<(usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut bare = None;
    let mut i = 0;
    while let Some(j) = text[i..].find(name) {
        let at = i + j;
        let end = at + name.len();
        let before_ok = at == 0 || !is_word(text[..at].chars().next_back()?);
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            // `.name` is the reference a rewrite wants; keep looking for one.
            if text[..at].ends_with('.') {
                return Some((at - 1, name.len() + 1));
            }
            if bare.is_none() {
                bare = Some((at, name.len()));
            }
        }
        i = end;
        if i >= text.len() {
            break;
        }
    }
    bare
}

/// A value a missing one can stand for, per scalar type.
fn scalar_default(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "string" => "\"\"",
        "int" => "0",
        "float" => "0.0",
        "bool" => "false",
        "date" => "currentDate",
        "timestamp" => "now",
        _ => return None,
    })
}

fn closest_token(text: &str, target: &str) -> Option<(usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut best: Option<(usize, usize, usize)> = None;
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (true, Some(_)) => {}
            (false, Some(s)) => {
                let token = &text[s..i];
                if token != target {
                    let d = cagara_hir::suggest::edit_distance(token, target);
                    let limit = cagara_hir::suggest::typo_limit(target);
                    if d <= limit && best.is_none_or(|(_, _, b)| d < b) {
                        best = Some((s, i, d));
                    }
                }
                start = None;
            }
            (false, None) => {}
        }
    }
    if let Some(s) = start {
        let token = &text[s..];
        if token != target {
            let d = cagara_hir::suggest::edit_distance(token, target);
            let limit = cagara_hir::suggest::typo_limit(target);
            if d <= limit && best.is_none_or(|(_, _, b)| d < b) {
                best = Some((s, text.len(), d));
            }
        }
    }
    best.map(|(s, e, _)| (s, e))
}

/// The type of every definition that does not write one, as an inlay hint after
/// its name.
///
/// An annotation is left alone: the user already said it, and a hint repeating
/// it is noise. A definition whose type is still open is skipped rather than
/// shown as `?` — the editor is not the place for the compiler's uncertainty.
pub fn type_hints(ws: &Workspace) -> Vec<(Range, String)> {
    let tc = check_reachable(ws);
    let md = &ws.modules[ws.root];
    let p = Positions::new(&md.text);
    let mut out = Vec::new();
    for (i, d) in md.module.defs.iter().enumerate() {
        if d.ty.is_some() {
            continue;
        }
        let Some(ty) = tc.type_of(ws.root, i) else {
            continue;
        };
        let Some((s, e)) = trimmed(&md.text, d.span) else {
            continue;
        };
        let name_end = (s + d.name.len()).min(e);
        out.push((root_range(&p, name_end, name_end), format!(": {ty}")));
    }
    out
}
