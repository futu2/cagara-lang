//! Workspace-owned reuse of immutable compilation results.
//!
//! Salsa memoizes exact facts for each definition. A changed fact invalidates
//! its transitive users; no hash serves as a proof of semantic equality. The
//! graph walks are iterative, including for recursive or very long programs.

use crate::check::{check, definition_facts, DefinitionFacts};
use crate::compile::{compile_reusing, Compilation, CompilerInput, DefinitionId};
use crate::workspace::Workspace;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) struct CachedCompilation {
    pub compilation: Compilation,
    facts: HashMap<DefinitionId, Arc<DefinitionFacts>>,
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

    let mut reuse = HashMap::new();
    if let Some(previous) = previous {
        let mut dirty: HashSet<_> = facts
            .iter()
            .filter(|(id, value)| previous.facts.get(id) != Some(value))
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
        for query in previous.compilation.queries {
            if !dirty.contains(&query.id) && facts.contains_key(&query.id) && query.result.is_ok() {
                reuse.insert(query.id, query.result);
            }
        }
    }
    CachedCompilation {
        compilation: compile_reusing(CompilerInput::new(ws, &tc), reuse),
        facts,
    }
}
