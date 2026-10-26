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

use crate::check::{check, definition_facts, DefinitionFacts};
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
    let tc = check(ws);
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
