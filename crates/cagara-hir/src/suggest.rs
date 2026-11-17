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
    let limit = typo_limit(name);
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

/// A name from another language, and what this one calls the same thing.
///
/// The edit-distance suggestion above answers "you mistyped something in scope".
/// It cannot answer the other case, and reports it worse than saying nothing:
/// `null` is *not* a typo for anything, and `did you mean `nothing`` is only
/// reachable by accident — the distance is 6. A reader arriving from SQL or
/// TypeScript writes these names because they are the names for the idea, so the
/// message says what the idea is called here.
///
/// The list is deliberately short and only contains names whose *meaning* carries
/// over. `if` is here because `ifThenElse` is the spelling; `let` is not, because
/// this language has `let … in` and the word is already a keyword.
pub(crate) fn from_another_language(name: &str) -> Option<&'static str> {
    Some(match name {
        // Absence.
        "null" | "nil" | "None" | "undefined" => {
            "`nothing` is the absent value, and `maybe a` \
             is its type; `??` supplies a default and `lift` maps inside one"
        }
        // Conditionals.
        "if" | "when" => {
            "a conditional is `ifThenElse p a b`, or a `case` expression for more \
             than two arms"
        }
        "else" => "a conditional is `ifThenElse p a b`; `case` has the arms",
        "then" => "a conditional is `ifThenElse p a b`, with the condition first",
        // Boolean spelling. `not` and `isNull` are *defined* here, so they never
        // reach this table; only the spellings this language does not have do.
        "and" => "`&&` is conjunction",
        "or" => "`||` is disjunction",
        // Aggregates.
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
            "aggregates are lowercase here: `count`, `sum`, `avg`, `min`, `max`"
        }
        // The date field extractors were `year`/`month`/`day` until the names
        // said *which field* rather than *which bucket*: `group (month .d)` was
        // `EXTRACT(MONTH …)`, so every January in the table landed in one row.
        // A reader who learned the old name needs to be told both the new name
        // and, if they wanted a bucket, the different function that gives one.
        "year" => {
            "the field is `yearOfDate`; a year *bucket* is `truncYear`, whose \
             result is a date"
        }
        "month" => {
            "the field is `monthOfYear`; a month *bucket* is `truncMonth`, whose \
             result is a date — `group (monthOfYear .d)` merges every January"
        }
        "day" => {
            "the field is `dayOfMonth`; a day *bucket* is `truncDay`. `dayOfWeek` \
             and `dayOfYear` are the other two fields"
        }
        // Set operators written as words.
        "UNION" | "JOIN" => {
            "a set operation is a stage: `& union`, `& unionAll`, `& intersect`, `& except`; a \
             join is `& innerJoin q on` and its relatives"
        }
        _ => return None,
    })
}

/// How far a name may be from a candidate and still count as a typo.
///
/// Loose for short names, tight for long ones: `id` is one edit from `in`, and
/// suggesting that would be noise; `amout` is one edit from `amount` and is the
/// whole point. Here rather than at each caller because the language server used to
/// compute it itself — the same expression, in a different crate, where it could
/// drift from this one without anything noticing.
pub fn typo_limit(name: &str) -> usize {
    (name.chars().count() / 3).max(1)
}

/// Edit distance over characters, counting a swapped pair as one edit.
///
/// This is optimal string alignment: plain Levenshtein counts `cuont` → `count` as
/// two edits (delete, insert), which is exactly at the bound for a five-letter name
/// and so produced *no* suggestion for the most common typo there is — swapping two
/// adjacent letters. Counting the swap once fixes that and `leftjion` → `leftJoin`
/// with it.
///
/// Three callers, one measurement: the near-miss for an unknown *name*, the
/// near-miss for an unknown *type* (which compares against every known type name,
/// so its threshold is on the distance rather than on the name's length), and the
/// language server, which picks a typo out of a completion range. It is public for
/// the last of those, which is a different crate.
///
/// It is not full Damerau-Levenshtein: that allows a transposition of non-adjacent
/// characters, which is not a shape a typo takes and would make unrelated short
/// names look close.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // Three rows, because a transposition reads the row before the previous one.
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let mut best = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            if i > 0 && j > 0 && *ca == b[j - 1] && a[i - 1] == *cb {
                best = best.min(prev2[j - 1] + 1);
            }
            cur[j + 1] = best;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{edit_distance, from_another_language, nearest, typo_limit};

    #[test]
    fn a_typo_is_suggested_and_a_short_name_is_not() {
        let names = ["amount", "user_id", "id", "in"];
        assert_eq!(nearest("amout", names), Some("amount"));
        assert_eq!(nearest("usr_id", names), Some("user_id"));
        // Two edits on a three-character name: too far to be a typo.
        assert_eq!(nearest("xyz", names), None);
    }

    /// A swapped pair is one typo, not two edits.
    ///
    /// Plain Levenshtein counts `cuont` → `count` as a delete and an insert, which
    /// is two — exactly past the bound for a five-letter name, so the most common
    /// typo there is produced *no* suggestion. Counting the transposition once is
    /// what makes `cuont`, `leftjion` and `form` → `from` all suggestible.
    #[test]
    fn a_swapped_pair_counts_as_one_edit() {
        for (typo, name, edits) in [
            ("cuont", "count", 1),
            // Two edits — the swap *and* the case — and still inside the bound,
            // which is the point: a long name tolerates both.
            ("leftjion", "leftJoin", 2),
            ("form", "from", 1),
            ("slect", "select", 1),
        ] {
            assert_eq!(edit_distance(typo, name), edits, "{typo} -> {name}");
            assert!(
                edit_distance(typo, name) <= typo_limit(typo),
                "{typo} must be suggestible"
            );
        }
        // A transposition of *non-adjacent* characters is not a typo shape, and
        // counting it would make unrelated names look close: `aXb` → `baX` swaps
        // characters two apart, which full Damerau-Levenshtein would count as one
        // and this does not.
        assert_eq!(edit_distance("axb", "bxa"), 2);
        // Two adjacent characters in a two-character name *are* adjacent, so that
        // one is a single swap.
        assert_eq!(edit_distance("ct", "tc"), 1);
        // Ordinary edits still measure as they did.
        assert_eq!(edit_distance("amout", "amount"), 1);
        assert_eq!(edit_distance("uppper", "upper"), 1);
        assert_eq!(edit_distance("abc", "abc"), 0);
    }

    /// The bound is one expression, shared with the language server.
    #[test]
    fn the_typo_bound_is_loose_for_short_names_and_tight_for_long() {
        assert_eq!(typo_limit("id"), 1);
        assert_eq!(typo_limit("abc"), 1);
        assert_eq!(typo_limit("amount"), 2);
        assert_eq!(typo_limit("user_identifier"), 5);
    }

    /// The borrowed-name table names the idea, not a near miss.
    #[test]
    fn a_borrowed_name_is_recognised() {
        for n in ["null", "nil", "if", "and", "or", "COUNT", "UNION"] {
            assert!(
                from_another_language(n).is_some(),
                "`{n}` should be recognised"
            );
        }
        // Names this language *has* must not be in the table: the checker resolves
        // them before it asks, so an entry would be dead code that reads as live.
        for n in ["not", "isNull", "nothing", "count", "union"] {
            assert!(
                from_another_language(n).is_none(),
                "`{n}` is a definition here, so it must not be borrowed"
            );
        }
        assert!(from_another_language("uppper").is_none());
    }
}
