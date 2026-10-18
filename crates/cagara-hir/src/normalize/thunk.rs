//! The `id` rule: a value that *is* the identity function.
//!
//! `id : a -> a = x => x` is two things at once. Where it is **applied**, β-
//! reduction removes it: `id v` is `v`. Where it is a **value** — the right
//! operand of a composition, `trim >>> id` — nothing applies it, so it has to be
//! recognised as a value that composes to nothing.
//!
//! The rule lives in [`Value::is_identity`]: one syntactic clause, a one-parameter
//! lambda whose body is its parameter, closing over no environment. Reducing an
//! applied identity and composing away an unapplied one are then the same fact
//! about the same value, which is why there is no separate pass.
//!
//! # Why an *inverse*
//!
//! The prelude writes `plain = (f) => (x) => f x`, and `_&_` is `plain` applied,
//! so a stage applied through the pipeline operator normalizes `plain` to a
//! closure and applies it twice. That reduces by β-reduction alone — no rule
//! here — and `_&_` is `x => f => f x` applied, which was always within the
//! language's core.
//!
//! # What is *not* here
//!
//! There is no state called "a function carried inside out". An earlier draft
//! of this module kept one, for `x => f x`-shaped values; it was the wrong
//! representation, because it made `apply` mean two different things depending
//! on a field of the normalizer. A lambda is a [`Value::Closure`] and applying it
//! is β-reduction, which is what [`super::Value`] states.

use super::Value;

/// Whether a value denotes the identity function.
#[allow(dead_code)] // The rule is exercised through Value::is_identity.
pub(crate) fn is_identity(value: &Value<'_>) -> bool {
    value.is_identity()
}
