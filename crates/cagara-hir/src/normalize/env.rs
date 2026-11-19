//! The reduction environment.
//!
//! Environments are the normalizer's substitution. A closure captures the
//! environment it was created in; applying it binds one more parameter; a local
//! name is a lookup. Nothing is ever substituted into syntax, so no argument can
//! be captured by a binder it was passed under; that follows from the
//! representation rather than requiring syntax substitution.

use super::Value;
use crate::check::{Choice, DefId, Instance, OverloadUse};
use std::sync::Arc;

/// The arguments bound so far, innermost last.
///
/// A frame chain rather than a map: a lookup walks from the innermost binder
/// outwards, which is the scoping rule itself, and binding a parameter copies
/// only the chain of pointers below it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Env<'a> {
    frames: Option<Arc<Frame<'a>>>,
    // Overload arguments are lexical, just like value arguments: two closures
    // made from the same helper may call different SQL templates.
    holes: Arc<[(DefId, Instance)]>,
    pub(crate) instance: Instance,
}

/// One binding and the frame it was pushed onto.
#[derive(Debug, PartialEq)]
struct Frame<'a> {
    name: String,
    value: Value<'a>,
    outer: Option<Arc<Frame<'a>>>,
}

impl<'a> Env<'a> {
    pub(crate) fn with_instance(instance: Instance) -> Self {
        Self {
            instance,
            ..Self::default()
        }
    }

    /// A fresh definition scope. Its hole k is the choice recorded at k of
    /// this particular use, resolved in the *caller's* scope.
    pub(crate) fn instantiate(
        &self,
        choices: &[OverloadUse],
        count: usize,
        instance: Instance,
    ) -> Option<Self> {
        let holes = (0..count)
            .map(|k| self.resolve(choices.get(k).filter(|u| u.hole == k)?))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            frames: None,
            holes: holes.into(),
            instance,
        })
    }

    pub(crate) fn resolve(&self, u: &OverloadUse) -> Option<(DefId, Instance)> {
        match u.choice {
            Choice::Def(module, def) => Some(((module, def), u.instance.at_use(&self.instance))),
            Choice::Hole(k) => self.holes.get(k).cloned(),
        }
    }

    /// The value bound to `name`, innermost binding first.
    pub(crate) fn lookup(&self, name: &str) -> Option<&Value<'a>> {
        let mut frame = self.frames.as_deref();
        while let Some(binding) = frame {
            if binding.name == name {
                return Some(&binding.value);
            }
            frame = binding.outer.as_deref();
        }
        None
    }

    /// This environment with one more binding.
    pub(crate) fn bind(&self, name: &str, value: Value<'a>) -> Env<'a> {
        Env {
            holes: self.holes.clone(),
            instance: self.instance.clone(),
            frames: Some(Arc::new(Frame {
                name: name.to_string(),
                value,
                outer: self.frames.clone(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_binding_shadows_an_earlier_one() {
        let env = Env::default()
            .bind("x", Value::Label(super::super::Label::text("first")))
            .bind("x", Value::Label(super::super::Label::text("second")));
        assert_eq!(
            env.lookup("x"),
            Some(&Value::Label(super::super::Label::text("second")))
        );
        assert_eq!(env.lookup("y"), None);
    }

    #[test]
    fn binding_does_not_disturb_the_environment_it_came_from() {
        let outer = Env::default().bind("x", Value::Label(super::super::Label::text("outer")));
        let inner = outer.bind("x", Value::Label(super::super::Label::text("inner")));
        assert_eq!(
            outer.lookup("x"),
            Some(&Value::Label(super::super::Label::text("outer")))
        );
        assert_eq!(
            inner.lookup("x"),
            Some(&Value::Label(super::super::Label::text("inner")))
        );
    }
}
