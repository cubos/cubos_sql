//! "Did you mean ..." helper used by diagnostics.
//!
//! Given a misspelled identifier and a list of candidates, returns the
//! closest match within a tolerance threshold — or `None` if no candidate is
//! close enough to be a useful suggestion.

/// Find the candidate closest to `query` by Levenshtein distance.
///
/// Returns the candidate together with its distance, only when the distance
/// is within a threshold that scales with the length of `query` (so short
/// names don't get matched against wildly different long names). The
/// threshold is roughly "1/3 of the query length, with a floor of 1 and a
/// cap of 3" — chosen to mirror Rust's own "did you mean" heuristic.
///
/// Case-insensitive: comparison happens on lowercase forms, but the returned
/// candidate is the original casing.
pub(crate) fn suggest_similar<'a, I>(query: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let q_lower = query.to_ascii_lowercase();
    let threshold = max_distance_for(query.len());

    let mut best: Option<(&'a str, usize)> = None;
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        let d = levenshtein(&q_lower, &cand.to_ascii_lowercase());
        if d <= threshold
            && let Some((_, best_d)) = best
            && d >= best_d
        {
            continue;
        }
        if d <= threshold {
            best = Some((cand, d));
        }
    }
    best.map(|(c, _)| c)
}

fn max_distance_for(len: usize) -> usize {
    match len {
        0..=2 => 1,
        3..=7 => 2,
        _ => 3,
    }
}

/// Classic Wagner–Fischer Levenshtein distance with two-row optimization.
fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.chars().count();
    }
    if b.is_empty() {
        return a.chars().count();
    }

    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr: Vec<usize> = vec![0; b_chars.len() + 1];

    for (i, ac) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b_chars.iter().enumerate() {
            let cost = if ac == bc { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typo_close_match() {
        let candidates = ["users", "posts", "comments"];
        assert_eq!(suggest_similar("userz", candidates), Some("users"));
        assert_eq!(suggest_similar("usres", candidates), Some("users"));
    }

    #[test]
    fn case_insensitive() {
        let candidates = ["Users", "Posts"];
        assert_eq!(suggest_similar("user", candidates), Some("Users"));
    }

    #[test]
    fn too_distant() {
        let candidates = ["users", "posts"];
        assert_eq!(suggest_similar("xyz", candidates), None);
    }

    #[test]
    fn short_names_have_tight_threshold() {
        // "x" and "y" are only 1 apart but threshold for len 1 is also 1 — should match
        let candidates = ["y"];
        assert_eq!(suggest_similar("x", candidates), Some("y"));
        // "xy" vs "ab" is distance 2, threshold for len 2 is 1 — no match
        let candidates = ["ab"];
        assert_eq!(suggest_similar("xy", candidates), None);
    }

    #[test]
    fn picks_closest() {
        let candidates = ["users", "user_logs", "userdata"];
        // "userz" is distance 1 from "users", 4 from "user_logs", 4 from "userdata".
        assert_eq!(suggest_similar("userz", candidates), Some("users"));
    }
}
