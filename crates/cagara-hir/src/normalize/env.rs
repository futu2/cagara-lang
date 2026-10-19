//! The reduction environment.
//!
//! Environments are the normalizer's substitution. A closure captures the
//! environment it was created in; applying it binds one more parameter; a local
//! name is a lookup. Nothing is ever substituted into syntax, so no argument can
//! be captured by a binder it was passed under; that follows from the
//! representation rather than requiring syntax substitution.

use super::Value;

/// The arguments bound so far, innermost last.
///
/// A frame chain rather than a map: a lookup walks from the innermost binder
/// outwards, which is the scoping rule itself, and binding a parameter copies
/// only the chain of pointers below it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Env<'a> {
    frames: Option<std::sync::Arc<Frame<'a>>>,
}

/// One binding and the frame it was pushed onto.
#[derive(Debug, PartialEq)]
struct Frame<'a> {
    name: String,
    value: Value<'a>,
    outer: Option<std::sync::Arc<Frame<'a>>>,
}

impl<'a> Env<'a> {
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
            frames: Some(std::sync::Arc::new(Frame {
                name: name.to_string(),
                value,
                outer: self.frames.clone(),
            })),
        }
    }

    /// Whether anything is bound.
    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_none()
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
