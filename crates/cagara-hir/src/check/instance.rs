//! Scalar type arguments retained for SQL templates after inference. Most SQL
//! is parametric; some SQL (such as a fixed-scale cast) needs the actual type.
//! These are substitutions the checker already made, not a second inference pass.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Instance {
    // Only scalar constructors, Nat, and the enclosing scheme's Gen indices.
    // Never checker-arena variables, rows, phases, or function types.
    args: BTreeMap<u32, Ty>,
    result: Option<Ty>,
}

#[derive(Default)]
pub(crate) struct PendingInstance {
    pub(crate) args: HashMap<u32, Ty>,
    pub(crate) result: Option<Ty>,
}

impl PendingInstance {
    /// Close the recorded substitution when the enclosing scheme is generalized.
    pub(crate) fn finish(self, ck: &Checker<'_>, gens: &HashMap<u32, u32>) -> Instance {
        fn scalar(t: &Ty, ck: &Checker<'_>, gens: &HashMap<u32, u32>) -> Option<Ty> {
            match ck.resolve(t) {
                Ty::Var(v) | Ty::Rigid(v, _) => Some(Ty::Gen(*gens.get(&v)?)),
                t @ Ty::Nat(_) => Some(t),
                Ty::Con(tag, xs)
                    if SCALARS.contains(&tag) || matches!(tag, "decimal" | "maybe" | "list") =>
                {
                    Some(Ty::Con(
                        tag,
                        xs.iter()
                            .map(|x| scalar(x, ck, gens))
                            .collect::<Option<_>>()?,
                    ))
                }
                _ => None,
            }
        }
        Instance {
            args: self
                .args
                .into_iter()
                .filter_map(|(k, t)| Some((k, scalar(&t, ck, gens)?)))
                .collect(),
            result: self.result.and_then(|t| scalar(&t, ck, gens)),
        }
    }
}

impl Instance {
    /// Resolve the callee's generic indices in the lexical caller's instance.
    pub(crate) fn at_use(&self, caller: &Instance) -> Self {
        fn subst(t: &Ty, args: &BTreeMap<u32, Ty>) -> Option<Ty> {
            match t {
                Ty::Gen(k) => args.get(k).cloned(),
                Ty::Con(tag, xs) => Some(Ty::Con(
                    tag,
                    xs.iter().map(|x| subst(x, args)).collect::<Option<_>>()?,
                )),
                t => Some(t.clone()),
            }
        }
        Instance {
            args: self
                .args
                .iter()
                .filter_map(|(k, t)| Some((*k, subst(t, &caller.args)?)))
                .collect(),
            result: self.result.as_ref().and_then(|t| subst(t, &caller.args)),
        }
    }

    pub(crate) fn result(&self) -> Option<ScalarType> {
        self.result
            .as_ref()
            .map(ty_to_scalar)
            .filter(ScalarType::is_concrete)
    }
}
