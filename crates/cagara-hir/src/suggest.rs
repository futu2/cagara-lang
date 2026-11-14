//! "Did you mean …?" for a name that is not the one in scope.
//!
//! Three places report a name the program does not have — an unknown definition
//! (`unknown name`), a column a stage cannot see, and a column `omit` or a join
//! side does not have — and each already knows the names that *are* there. This
//! is the one place the near-miss is chosen, so the three agree on the rule and
//! on the suffix the language server turns into a rename (it looks for exactly
//! `did you mean `X`` in the message).

/// The closest candidate to `name`, if one is close enough to be a typo.
///
/// The bound is loose for short names and tight for long ones: `id` is one edit
/// from `in`, and suggesting that would be noise; `amout` is one edit from
/// `amount` and is the whole point.
pub(crate) fn nearest<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .filter(|c| *c != name)
        .map(|c| (c, edit_distance(name, c)))
        .filter(|(_, d)| *d <= limit)
        .min_by_key(|(c, d)| (*d, std::cmp::Reverse(c.len())))
        .map(|(c, _)| c)
}

/// The "; did you mean `X`?" suffix, or nothing when no candidate is close.
pub(crate) fn did_you_mean<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> String {
    match nearest(name, candidates) {
        Some(c) => format!("; did you mean `{c}`?"),
        None => String::new(),
    }
}

/// Levenshtein distance, over characters.
///
/// Three callers, one measurement: the near-miss for an unknown *name* (below), the
/// near-miss for an unknown *type* (the checker compares against every known type
/// name, so its threshold is on the distance rather than on the name's length), and
/// the language server, which picks a typo out of a completion range and would
/// otherwise carry a third copy. It is public for the last of those, which is a
/// different crate.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::nearest;

    #[test]
    fn a_typo_is_suggested_and_a_short_name_is_not() {
        let names = ["amount", "user_id", "id", "in"];
        assert_eq!(nearest("amout", names), Some("amount"));
        assert_eq!(nearest("usr_id", names), Some("user_id"));
        // Two edits on a three-character name: too far to be a typo.
        assert_eq!(nearest("xyz", names), None);
    }
}
