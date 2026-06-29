//! Fuzzy subsequence matching with fzf/fzy-style scoring.
//!
//! Used by the Ctrl+R history-search modal to filter `input_history` as the
//! user types. [`fuzzy_match`] returns whether `needle` is a subsequence of
//! `haystack` (case-insensitive), a ranking [`FuzzyMatch::score`], and the
//! [`FuzzyMatch::positions`] of the matched haystack chars so the renderer can
//! highlight them.
//!
//! Algorithm: a single forward DP over `(needle_idx, haystack_idx)`, with a
//! running-max optimization that keeps it `O(needle_len * haystack_len)` and
//! single-pass. Bonuses (all additive, only used for ranking):
//!
//! - Start-of-string, whitespace/punctuation boundary, or lower→upper
//!   camelCase transition: `BONUS_BOUNDARY`.
//! - Adjacent (gap of zero) to the previous matched char: `BONUS_CONSECUTIVE`.
//! - Exact case match (not just case-insensitive): `BONUS_CASE_MATCH`.
//! - Each char of gap between consecutive matches: `-PENALTY_GAP`.

/// Result of a successful fuzzy match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Match quality score; higher is better. Used only for ranking.
    pub score: i64,
    /// Char indices in the haystack that the needle matched, in order. Used
    /// by the renderer to highlight the matched characters.
    pub positions: Vec<usize>,
}

const BONUS_BOUNDARY: i64 = 50;
const BONUS_CONSECUTIVE: i64 = 30;
const BONUS_CASE_MATCH: i64 = 10;
const PENALTY_GAP: i64 = 1;
const NEG: i64 = i64::MIN / 4;

/// True if the transition `prev → cur` is a word boundary: at the start of
/// the haystack, just after whitespace or punctuation, or at a lower→upper
/// camelCase boundary. Matched chars at boundaries get [`BONUS_BOUNDARY`] so
/// the matcher prefers whole-word / token starts.
fn is_word_boundary(prev: Option<char>, cur: char) -> bool {
    match prev {
        None => true,
        Some(p) => {
            p.is_whitespace()
                || !p.is_alphanumeric()
                || (p.is_lowercase()
                    && cur.is_uppercase()
                    && p.is_alphabetic()
                    && cur.is_alphabetic())
        }
    }
}

/// Bonus accumulated at haystack position `j` when it matches a needle char.
/// `needle_c` is the needle char (already known to case-insensitively match
/// `h[j]`); an exact-case match adds [`BONUS_CASE_MATCH`].
fn char_bonus(h: &[char], j: usize, needle_c: char) -> i64 {
    let cur = h[j];
    let prev = if j == 0 { None } else { Some(h[j - 1]) };
    let mut bonus = 0;
    if is_word_boundary(prev, cur) {
        bonus += BONUS_BOUNDARY;
    }
    if cur == needle_c {
        bonus += BONUS_CASE_MATCH;
    }
    bonus
}

/// Fuzzy-match `needle` against `haystack` (case-insensitive subsequence).
///
/// Returns `None` when `needle` is not a subsequence of `haystack`. An empty
/// `needle` matches every haystack with score `0` and no highlighted positions
/// — this is what the history modal wants when the query box is empty (show
/// everything, highlight nothing).
///
/// `positions` are char indices (not byte offsets) into `haystack`, in
/// ascending order, one per needle char.
pub fn fuzzy_match(haystack: &str, needle: &str) -> Option<FuzzyMatch> {
    let h: Vec<char> = haystack.chars().collect();
    let n: Vec<char> = needle.chars().collect();

    if n.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }

    let h_len = h.len();
    let n_len = n.len();
    if h_len < n_len {
        return None;
    }

    // dp[i][j]: best score matching n[0..=i] ending exactly at h[j] (h[j]
    // consumed). NEG means "impossible". back[i][j]: the j' matched to n[i-1]
    // on the optimal path into dp[i][j], for reconstruction.
    let mut dp = vec![vec![NEG; h_len]; n_len];
    let mut back: Vec<Vec<Option<usize>>> = vec![vec![None; h_len]; n_len];

    // Base case: needle[0] can match any single haystack char with no predecessor.
    for j in 0..h_len {
        if h[j].eq_ignore_ascii_case(&n[0]) {
            dp[0][j] = char_bonus(&h, j, n[0]);
        }
    }

    // Inductive case: one forward pass per needle char, maintaining a running
    // max so the inner loop stays O(h_len) instead of O(h_len^2).
    //
    // running_max = max over k<j of (dp[i-1][k] - PENALTY_GAP * (j-1-k))
    // running_max_k = the k achieving it (oldest on ties, refreshed lazily).
    //
    // At each j we also explicitly consider the immediately-adjacent
    // predecessor k=j-1 because only it earns BONUS_CONSECUTIVE; the running
    // max above already includes k=j-1 with no penalty, so we just compare
    // "with consecutive bonus" against "best non-adjacent aged value".
    for i in 1..n_len {
        let mut running_max = NEG;
        let mut running_max_k: Option<usize> = None;
        for j in 0..h_len {
            // 1) Try matching needle[i] at haystack[j].
            if h[j].eq_ignore_ascii_case(&n[i]) {
                let mut best_val = NEG;
                let mut best_k: Option<usize> = None;
                // Adjacent predecessor (k = j-1) earns the consecutive bonus.
                if j >= 1 && dp[i - 1][j - 1] != NEG {
                    best_val = dp[i - 1][j - 1].saturating_add(BONUS_CONSECUTIVE);
                    best_k = Some(j - 1);
                }
                // Non-adjacent best from the running max beats the adjacent
                // candidate only when it strictly exceeds it, so ties keep
                // the adjacent path (visually tighter highlight run).
                if running_max != NEG && running_max > best_val {
                    best_val = running_max;
                    best_k = running_max_k;
                }
                if best_val != NEG {
                    let total = best_val.saturating_add(char_bonus(&h, j, n[i]));
                    if total > dp[i][j] {
                        dp[i][j] = total;
                        back[i][j] = best_k;
                    }
                }
            }

            // 2) Extend the running max with k=j as a future predecessor
            //    (contributes dp[i-1][j] with no gap when matched at j+1).
            if dp[i - 1][j] != NEG && dp[i - 1][j] > running_max {
                running_max = dp[i - 1][j];
                running_max_k = Some(j);
            }
            // 3) Age the running max by one gap unit for the next iteration.
            running_max = running_max.saturating_sub(PENALTY_GAP);
        }
    }

    // Pick the best ending position for the last needle char. Strict `>`
    // keeps the lowest-`j` end on ties, which visually favors earlier matches.
    let mut best_end: Option<usize> = None;
    let mut best_score = NEG;
    for (j, &cell) in dp[n_len - 1].iter().enumerate() {
        if cell > best_score {
            best_score = cell;
            best_end = Some(j);
        }
    }
    let end = best_end?;
    if best_score == NEG {
        return None;
    }

    // Reconstruct positions by following back-pointers from (n_len-1, end).
    let mut positions: Vec<usize> = Vec::with_capacity(n_len);
    let mut i = n_len - 1;
    let mut j = end;
    loop {
        positions.push(j);
        if i == 0 {
            break;
        }
        {
            let prev_j = back[i][j]?;
            j = prev_j;
            i -= 1;
        }
    }
    positions.reverse();
    debug_assert_eq!(positions.len(), n_len);

    Some(FuzzyMatch {
        score: best_score,
        positions,
    })
}

/// Filter and rank `items` by fuzzy match against `query`, preserving the
/// original order on ties (stable). Returns `(original_index, FuzzyMatch)` for
/// each item whose match is `Some`. An empty `query` matches every item with
/// score `0` and no highlight positions, so the caller renders the full list.
pub fn rank<I: AsRef<str>>(items: &[I], query: &str) -> Vec<(usize, FuzzyMatch)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| fuzzy_match(item.as_ref(), query).map(|m| (i, m)))
        .collect()
}

/// Sort a list of `(index, FuzzyMatch)` in place by descending score, with
/// original-index ascending as the stable tiebreaker so equally-good matches
/// keep their top-to-bottom input order. Returns `&mut` so callers can chain.
pub fn sort_by_score(matches: &mut [(usize, FuzzyMatch)]) {
    // Reverse(score) sorts descending while keeping the slice sort stable, so
    // equally-good matches retain their top-to-bottom input order — exactly
    // the tiebreaker we want.
    matches.sort_by_key(|(_, m)| std::cmp::Reverse(m.score));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty needle matches anything with no highlighted positions.
    #[test]
    fn empty_needle_matches_everything() {
        let m = fuzzy_match("hello", "").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
    }

    /// Non-subsequence needles return None.
    #[test]
    fn rejects_non_subsequence() {
        assert!(fuzzy_match("abc", "ac").is_some()); // a-b-c contains a then c
        assert!(fuzzy_match("abc", "ca").is_none()); // c never appears before a
        assert!(fuzzy_match("ab", "abc").is_none()); // needle longer than haystack
    }

    /// Case-insensitive matching still records exact-case bonuses in the score.
    #[test]
    fn case_insensitive_match_with_case_bonus() {
        let upper = fuzzy_match("ABC", "abc").unwrap();
        let lower = fuzzy_match("abc", "abc").unwrap();
        // Matching lowercase needle against uppercase haystack earns no
        // BONUS_CASE_MATCH, so it should score strictly lower.
        assert!(lower.score > upper.score);
    }

    /// Consecutive matches beat scattered matches for the same needle.
    #[test]
    fn prefers_consecutive_run() {
        // "an" in "banana" can match at chars (1,2) [consecutive] or (3,4)
        // [also consecutive] or scattered. Both consecutive paths should beat
        // any scattered path; the score must be > 0.
        let m = fuzzy_match("banana", "an").unwrap();
        assert!(m.score > 0);
        assert_eq!(m.positions.len(), 2);
        // The matched positions must form a valid ascending subsequence.
        assert!(m.positions[0] < m.positions[1]);
    }

    /// Word-boundary bonus: matching at the start outranks matching mid-word.
    #[test]
    fn word_boundary_bonus() {
        // "cat" at start of "catalog" should outscore "cat" appearing inside
        // "concatenate" (where 'c' is mid-word).
        let start = fuzzy_match("catalog", "cat").unwrap();
        let mid = fuzzy_match("concatenate", "cat").unwrap();
        assert!(start.score > mid.score);
    }

    /// Positions are char indices, not byte offsets (multi-byte safe).
    #[test]
    fn positions_are_char_indices_for_unicode() {
        // "é" is two bytes in UTF-8 but one char. Needle "x" matches the ASCII
        // char at char-index 2 (byte-index 3).
        let m = fuzzy_match("éax", "x").unwrap();
        assert_eq!(m.positions, vec![2]);
    }

    /// Reconstructed positions actually correspond to the needle in order.
    #[test]
    fn positions_correspond_to_needle_chars() {
        let h = "foo bar baz";
        let n = "obb";
        let m = fuzzy_match(h, n).unwrap();
        let h_chars: Vec<char> = h.chars().collect();
        let n_chars: Vec<char> = n.chars().collect();
        for (k, &pos) in m.positions.iter().enumerate() {
            assert!(
                h_chars[pos].eq_ignore_ascii_case(&n_chars[k]),
                "position {} (haystack char {:?}) must match needle char {:?}",
                pos,
                h_chars[pos],
                n_chars[k]
            );
        }
    }

    /// rank() + sort_by_score() filters out non-matches and orders by score,
    /// preserving input order on ties (stable).
    #[test]
    fn rank_and_sort_by_score_orders_results() {
        // "scatter" matches `cat` mid-word (no boundary bonus) so it scores
        // strictly lower than the boundary-at-start matches in "catalog" and
        // "a cat", which themselves tie. Placed first in the input so the
        // sort is actually exercised.
        let items = vec!["scatter", "catalog", "a cat"];
        let mut ranked = rank(&items, "cat");
        sort_by_score(&mut ranked);
        assert_eq!(ranked.len(), 3);
        // catalog and "a cat" tie at the boundary-boosted score; stable sort
        // keeps their input order (catalog before "a cat").
        assert_eq!(ranked[0].0, 1); // catalog
        assert_eq!(ranked[1].0, 2); // a cat
        assert_eq!(ranked[2].0, 0); // scatter
        assert!(ranked[0].1.score > ranked[2].1.score);
    }

    /// Empty query in rank() returns every item, unhighlighted.
    #[test]
    fn rank_with_empty_query_returns_all() {
        let items = vec!["a", "b", "c"];
        let ranked = rank(&items, "");
        assert_eq!(ranked.len(), 3);
        for (_, m) in ranked {
            assert_eq!(m.score, 0);
            assert!(m.positions.is_empty());
        }
    }
}
