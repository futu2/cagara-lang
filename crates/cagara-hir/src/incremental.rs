//! Workspace-owned reuse of immutable compilation results.
//!
//! Salsa memoizes exact facts for each definition. A changed fact invalidates
//! its transitive users; no hash serves as a proof of semantic equality. The
//! graph walks are iterative, including for recursive or very long programs.
//!
//! Reuse is per definition, and the facts that authorize it say nothing about
//! *where* a definition sits (see [`DefinitionFacts`]). A definition that only
//! moved is therefore reused, and its cached result is *relocated*: the
//! locations it carries are offsets into the text it was compiled against, and
//! a definition below an insertion starts a few bytes further along in the text
//! as it is now. [`Layout`] is the record of where every definition sat when a
//! compilation ran, which is what makes that translation a lookup.
//!
//! Without it, a keystroke near the top of a file re-elaborated every
//! definition below it, because all of their spans had shifted — the common
//! case when typing, and the reason the incremental cache looked warm and
//! behaved cold.

use crate::check::{check_reachable, definition_facts, DefinitionFacts};
use crate::compile::{compile_reusing, Compilation, CompilerInput, DefinitionId};
use crate::ir::{Loc, Rel};
use crate::workspace::Workspace;
use cagara_syntax::ast::Span;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) struct CachedCompilation {
    pub compilation: Compilation,
    facts: HashMap<DefinitionId, Arc<DefinitionFacts>>,
    /// Where each module's definitions were when this compilation ran.
    layout: Layout,
}

/// Where one module's definitions sat in its text: their names, in order, and
/// their spans.
///
/// The names are what make the spans usable as a translation: a location is
/// placed through the definition it was written in, and an index only means the
/// same definition as long as the names in that position are the same.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModuleLayout {
    names: Vec<String>,
    spans: Vec<Span>,
}

type Layout = HashMap<usize, ModuleLayout>;

fn layout_of(ws: &Workspace) -> Layout {
    (0..ws.modules.len())
        .map(|m| {
            let defs = &ws.modules[m].module.defs;
            let layout = ModuleLayout {
                names: defs.iter().map(|d| d.name.clone()).collect(),
                spans: defs.iter().map(|d| d.span).collect(),
            };
            (m, layout)
        })
        .collect()
}

/// Where the definitions of an earlier compilation are now.
struct Moves<'a> {
    old: &'a Layout,
    new: &'a Layout,
    /// Modules whose definition list is still the same, by name and order.
    compatible: HashMap<usize, bool>,
}

impl<'a> Moves<'a> {
    fn new(old: &'a Layout, new: &'a Layout) -> Self {
        let compatible = old
            .iter()
            .map(|(&module, was)| {
                let same = new.get(&module).is_some_and(|now| {
                    now.names == was.names && now.spans.len() == was.spans.len()
                });
                (module, same)
            })
            .collect();
        Moves {
            old,
            new,
            compatible,
        }
    }

    /// `span`, as an offset into the text as it is now.
    ///
    /// `None` when it cannot be placed: a module that is gone, a definition
    /// list that changed, or a location before the first definition of a module
    /// that has one. A `None` is a refusal to reuse, never a guess — a location
    /// that named the wrong line would be worse than a rebuilt query.
    fn span(&self, module: usize, span: Span) -> Option<Span> {
        // `Span::default()` is "no location", not the first byte of the file.
        if span == Span::default() {
            return Some(span);
        }
        if !self.compatible.get(&module).copied().unwrap_or(false) {
            return None;
        }
        let was = self.old.get(&module)?;
        let now = self.new.get(&module)?;
        // The definition the location was written in: the last one that starts
        // at or before it. Binary search, not a scan — a file's locations are
        // many and its definitions are many, and one scan per location made
        // relocation quadratic in the size of the file.
        let anchor = was
            .spans
            .partition_point(|d| d.start <= span.start)
            .checked_sub(1)?;
        let delta = i64::from(now.spans[anchor].start) - i64::from(was.spans[anchor].start);
        Some(moved(span, delta))
    }
}

/// A span moved `delta` bytes, clamped at the start of the text.
fn moved(span: Span, delta: i64) -> Span {
    let at = |offset: u32| (i64::from(offset) + delta).max(0) as u32;
    Span {
        start: at(span.start),
        end: at(span.end),
    }
}

/// Move every location in `rel` onto the current text, or `None` if one cannot
/// be placed.
///
/// Every node of a reused relation carries the location it was written at, and
/// a location is an offset. A definition that moved leaves them all pointing
/// into text that is no longer there — at positions that may still be inside
/// the file, and may still be inside *some* definition, which is why the
/// translation is made by definition and refused when it cannot be.
fn relocate(rel: &mut Rel, moves: &Moves<'_>) -> Option<()> {
    if let Rel::At(Loc { module, span }, _) = rel {
        *span = moves.span(*module, *span)?;
    }
    for child in rel.children_mut() {
        relocate(child, moves)?;
    }
    Some(())
}

pub(crate) fn compile_current(
    ws: &Workspace,
    previous: Option<CachedCompilation>,
) -> CachedCompilation {
    let tc = check_reachable(ws);
    let mut facts = HashMap::new();
    let mut users: HashMap<DefinitionId, Vec<DefinitionId>> = HashMap::new();
    let mut pending: Vec<_> = (0..ws.modules[ws.root].module.defs.len())
        .map(|def| DefinitionId {
            module: ws.root,
            def,
        })
        .collect();
    while let Some(id) = pending.pop() {
        if facts.contains_key(&id) {
            continue;
        }
        let value = Arc::clone(definition_facts(&ws.db, ws.inputs[id.module], id.def));
        for &dependency in &value.dependencies {
            users.entry(dependency).or_default().push(id);
            pending.push(dependency);
        }
        facts.insert(id, value);
    }

    let layout = layout_of(ws);
    let mut reuse = HashMap::new();
    if let Some(previous) = previous {
        // The pointer comparison comes first: `definition_facts` hands back the
        // same allocation for a definition it has not re-run, which is the
        // common case and answers "unchanged" without walking the facts. A
        // definition with no facts from the previous compilation is new, and
        // new is changed.
        let mut dirty: HashSet<_> = facts
            .iter()
            .filter(|(id, value)| match previous.facts.get(id) {
                None => true,
                Some(was) => !Arc::ptr_eq(was, value) && **was != ***value,
            })
            .map(|(&id, _)| id)
            .collect();
        pending.extend(dirty.iter().copied());
        while let Some(id) = pending.pop() {
            for &user in users.get(&id).into_iter().flatten() {
                if dirty.insert(user) {
                    pending.push(user);
                }
            }
        }
        let moves = Moves::new(&previous.layout, &layout);
        for query in previous.compilation.queries {
            if dirty.contains(&query.id) || !facts.contains_key(&query.id) {
                continue;
            }
            let Ok(mut rel) = query.result else {
                continue;
            };
            if relocate(&mut rel, &moves).is_some() {
                reuse.insert(query.id, Ok(rel));
            }
        }
    }
    CachedCompilation {
        compilation: compile_reusing(CompilerInput::new(ws, &tc), reuse),
        facts,
        layout,
    }
}

#[cfg(test)]
mod consistency_tests {
    use crate::compile::compile_diagnostics;
    use crate::workspace::Workspace;
    use std::path::Path;

    /// Compiling after an edit must agree with compiling from scratch.
    ///
    /// The incremental path reuses a query's lowered relation when nothing it
    /// depends on changed, and *relocates* the spans in that reused tree when the
    /// text above it moved. Every editor session runs it, and it had one test — for
    /// the span arithmetic, not for the agreement. This is the property that
    /// matters: whatever an edit produces, a fresh workspace with the same text
    /// produces too.
    ///
    /// The edits are the ones that move spans without changing meaning (a blank
    /// line above, a comment), and the ones that change meaning through a
    /// *dependency* (a table's column renamed, a helper's body changed).
    fn messages(text: &str) -> Vec<String> {
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), text.to_string());
        compile_diagnostics(&ws)
            .iter()
            .map(|d| {
                format!(
                    "{}:{}:{}: {}",
                    d.line,
                    d.col,
                    d.severity.as_str(),
                    d.message
                )
            })
            .collect()
    }

    /// The diagnostics of the *edited* workspace — the incremental answer.
    ///
    /// Every edit compiles, so the cache the next step sees is the one the previous
    /// step produced: this is the path an editor takes, not a fresh read.
    fn messages_after_edits(edits: &[&str]) -> Vec<String> {
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), String::new());
        let mut out = Vec::new();
        for text in edits {
            assert!(
                ws.set_source(ws.root, (*text).to_string()),
                "an edit was rejected"
            );
            // Each step compiles, so the cache is what the next step sees.
            out = compile_diagnostics(&ws)
                .iter()
                .map(|d| {
                    format!(
                        "{}:{}:{}: {}",
                        d.line,
                        d.col,
                        d.severity.as_str(),
                        d.message
                    )
                })
                .collect();
        }
        out
    }

    const BASE: &str = "users : query { id = int, name = string } = table \"s\" \"u\"\n\
                        q = users & select { id = .id }\n";

    #[test]
    fn an_edit_that_only_moves_text_agrees_with_a_fresh_compile() {
        // A blank line above everything: every span moves, nothing changes.
        let moved = format!("\n\n{BASE}");
        assert_eq!(messages_after_edits(&[BASE, &moved]), messages(&moved));
        // A comment line, which the formatter keeps and spans must skip.
        let commented = format!("# a note\n{BASE}");
        assert_eq!(
            messages_after_edits(&[BASE, &commented]),
            messages(&commented)
        );
    }

    #[test]
    fn an_edit_that_breaks_a_dependency_agrees_with_a_fresh_compile() {
        // Renaming the table's column breaks `q`, and the error has to be
        // *reported*: a reused query whose dependency changed is exactly what the
        // dirty-propagation exists for.
        let broken = BASE.replace("id = int, name = string", "ident = int, name = string");
        assert_eq!(messages_after_edits(&[BASE, &broken]), messages(&broken));
        assert!(
            messages(&broken).iter().any(|m| m.contains("id")),
            "the rename has to be reported: {:?}",
            messages(&broken)
        );
    }

    /// Every span in a reused query matches the one a fresh compile produces.
    ///
    /// This is the property the relocation exists for, and diagnostics cannot see
    /// it: a reused query passed schema validation before, so it reports no error
    /// at a stale span — the span is simply wrong for whoever reads it next (hover,
    /// go-to-definition, a later pass). Comparing the trees directly is the only
    /// way to check it, and the check is exact: `Debug` prints every `Loc`.
    fn spans(text: &str) -> Vec<String> {
        let ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), text.to_string());
        let compilation = crate::compile::compile(&ws);
        compilation
            .queries
            .iter()
            .map(|q| match &q.result {
                Ok(rel) => format!("{}: {rel:?}", q.name),
                Err(d) => format!("{}: ERR {}", q.name, d.message),
            })
            .collect()
    }

    #[test]
    fn a_reused_query_carries_the_spans_a_fresh_compile_would_give_it() {
        let mut ws = Workspace::open_with(Path::new("/nonexistent/main.cagara"), String::new());
        for text in [BASE, &format!("\n\n\n{BASE}")] {
            assert!(
                ws.set_source(ws.root, text.to_string()),
                "an edit was rejected"
            );
            // Compile through the cache, which is what an editor does.
            let _ = compile_diagnostics(&ws);
        }
        let moved = format!("\n\n\n{BASE}");
        // The cached answer, then the slow one, for the same text.
        let cached = {
            let compilation = crate::compile::compile(&ws);
            compilation
                .queries
                .iter()
                .map(|q| match &q.result {
                    Ok(rel) => format!("{}: {rel:?}", q.name),
                    Err(d) => format!("{}: ERR {}", q.name, d.message),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(cached, spans(&moved), "the reused spans are stale");
    }

    #[test]
    fn an_edit_that_fixes_an_error_clears_it() {
        let broken = BASE.replace("select { id = .id }", "select { id = .nope }");
        // Broken first, then fixed: the diagnostic must not survive the fix.
        assert!(
            messages(&broken).iter().any(|m| m.contains("nope")),
            "{:?}",
            messages(&broken)
        );
        assert_eq!(
            messages_after_edits(&[&broken, BASE]),
            messages(BASE),
            "the fixed text has no diagnostics"
        );
        assert!(messages(BASE).is_empty(), "{:?}", messages(BASE));
    }
}

#[cfg(test)]
mod tests {
    use super::moved;
    use cagara_syntax::ast::Span;

    #[test]
    fn a_move_is_clamped_at_the_start_of_the_text() {
        let span = Span { start: 2, end: 6 };
        assert_eq!(moved(span, 3), Span { start: 5, end: 9 });
        assert_eq!(moved(span, -2), Span { start: 0, end: 4 });
        assert_eq!(moved(span, -9), Span { start: 0, end: 0 });
    }
}
