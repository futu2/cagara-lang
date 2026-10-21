//! The normalizer's budget.
//!
//! A pure functional language with definitions that may be used many times can
//! expand without bound: a hundred definitions that each use their argument
//! twice is a tree of 2^100 applications before anything is refused. A compiler
//! therefore has to *charge* for its work, and the charge has to happen where
//! the work happens rather than on the finished answer — the old pipeline's
//! size limit was checked after the expansion it was meant to prevent.
//!
//! Two costs are counted, because they bound two different shapes:
//!
//! * **recursion** — how many definitions down one normalization path. A chain of
//!   definitions that merely name each other is linear in this, and a recursive
//!   definition is unbounded, so this is also where a cycle is caught.
//! * **work** — nodes visited. A definition that uses its argument twice
//!   doubles the work at every level, so this is the bound that catches the
//!   exponential shape, and it is charged for every node the reduction enters,
//!   *before* the operation that node heads is applied.
//!
//! Exceeding either is a capability gap, not a program error: the program is
//! well-typed and the compiler cannot build it within its limits.

use crate::core::Origin;

use super::Control;

/// How many definitions may nest on one normalization path.
///
/// This limit is generous enough for the deepest helper chains the corpus and
/// examples use while still refusing a cycle immediately.
pub(crate) const MAX_DEPTH: usize = 200;

/// How many nodes one definition's normalization may visit or construct.
///
/// This limit, counted as work, refuses an exponential expansion *during*
/// normalization.
pub(crate) const MAX_WORK: usize = 200_000;

/// The budget for one definition's normalization.
#[derive(Debug, Clone)]
pub(crate) struct Budget {
    depth: usize,
    work: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new()
    }
}

impl Budget {
    pub(crate) fn new() -> Self {
        Budget { depth: 0, work: 0 }
    }

    /// Account for entering a definition.
    pub(crate) fn enter(&mut self, origin: Origin) -> Result<(), Control> {
        if self.depth >= MAX_DEPTH {
            return Err(Control::unsupported(
                origin,
                format!(
                    "this definition expands more than {MAX_DEPTH} definitions deep; \
                     recursion is not supported"
                ),
            ));
        }
        self.depth += 1;
        Ok(())
    }

    /// Account for leaving a definition.
    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Account for visiting one node.
    pub(crate) fn spend(&mut self, origin: Origin) -> Result<(), Control> {
        self.work += 1;
        if self.work > MAX_WORK {
            return Err(Control::unsupported(
                origin,
                format!(
                    "this definition expands to more than {MAX_WORK} normalization steps; \
                     it is too large to normalize"
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagara_syntax::ast::Span;

    fn origin() -> Origin {
        Origin::new(0, Span::default())
    }

    #[test]
    fn work_is_charged_and_refused_at_the_limit() {
        let mut budget = Budget::new();
        for _ in 0..MAX_WORK {
            budget.spend(origin()).expect("within the budget");
        }
        let refusal = budget.spend(origin()).expect_err("past the budget");
        assert!(matches!(refusal, Control::Unsupported { .. }));
    }

    #[test]
    fn depth_is_charged_and_released() {
        let mut budget = Budget::new();
        for _ in 0..MAX_DEPTH {
            budget.enter(origin()).expect("within the depth");
        }
        assert!(budget.enter(origin()).is_err(), "one definition too deep");
        budget.leave();
        budget.enter(origin()).expect("the depth was released");
    }
}
