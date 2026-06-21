// SPDX-License-Identifier: Apache-2.0

//! Fuzzy "find by name" search over the current folder's filenames. Pure logic, no GPU, no I/O —
//! fully unit-testable. The viewer feeds it the playlist filenames plus a query string; it returns
//! the best-matching playlist indices, ranked best-first.
//!
//! Matching is **subsequence-based** (case-insensitive): the query characters must appear in the
//! name in order, but not necessarily contiguous — so `img2` matches `my_image_2024.jpg`. Scoring
//! favours contiguous runs, matches at word boundaries (start, or after a separator), and a match
//! at the very start; shorter names break ties. This is the classic light fuzzy-finder behaviour,
//! cheap enough to re-run on every keystroke over a whole folder.

/// Score `name` against `query` (both compared case-insensitively). Returns `None` when the query
/// is not a subsequence of the name; otherwise a score where **higher is better**.
fn score(query: &str, name: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let n: Vec<char> = name.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut qi = 0usize;
    let mut s = 0i32;
    let mut prev_match: Option<usize> = None;
    for (i, &c) in n.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            s += 1; // base point per matched char
            if prev_match.is_some_and(|p| p + 1 == i) {
                s += 8; // contiguous with the previous match
            }
            let at_boundary =
                i == 0 || matches!(n.get(i - 1), Some(' ' | '_' | '-' | '.'));
            if at_boundary {
                s += 6; // start of a word
            }
            if i == 0 {
                s += 4; // very start of the name
            }
            prev_match = Some(i);
            qi += 1;
        }
    }
    if qi == q.len() {
        // Slight reward for shorter names (a tighter match).
        Some(s - (n.len() as i32) / 8)
    } else {
        None
    }
}

/// Rank `names` against `query`, returning at most `limit` playlist indices best-first.
///
/// An empty (or whitespace-only) query returns the first `limit` indices in playlist order, so the
/// modal shows a useful initial list before the user types anything.
pub fn search(query: &str, names: &[&str], limit: usize) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..names.len().min(limit)).collect();
    }
    let mut scored: Vec<(usize, i32)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, name)| score(q, name).map(|sc| (i, sc)))
        .collect();
    // Best score first; then shorter name; then original order for stability.
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| names[a.0].len().cmp(&names[b.0].len()))
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.into_iter().take(limit).map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_only_matches_in_order() {
        assert!(score("img", "image.png").is_some()); // i-m-g present in order
        assert!(score("img2", "my_image_2024.jpg").is_some()); // scattered but ordered
        assert!(score("gmi", "image.png").is_none()); // wrong order
        assert!(score("xyz", "image.png").is_none()); // chars absent
    }

    #[test]
    fn case_insensitive() {
        assert!(score("IMG", "img_0001.JPG").is_some());
    }

    #[test]
    fn contiguous_and_boundary_outrank_scattered() {
        // "cat" contiguous at a boundary should beat the same letters scattered.
        let tight = score("cat", "cat.png").unwrap();
        let loose = score("cat", "c_x_a_x_t.png").unwrap();
        assert!(tight > loose, "tight={tight} loose={loose}");
    }

    #[test]
    fn search_ranks_and_limits() {
        let names = ["background.jpg", "image.png", "x_img_2.png", "cat.gif"];
        // "img" matches image.png, x_img_2.png (and background.jpg? b-a-c-k-g-r... no 'i' before 'm'
        // -> no). Top result should be one of the img-bearing names, not cat.gif.
        let hits = search("img", &names, 2);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|&i| names[i] != "cat.gif"));
        // The contiguous, boundary "_img_" should rank above "image" (i-m-...-g scattered).
        assert_eq!(names[hits[0]], "x_img_2.png");
    }

    #[test]
    fn empty_query_returns_prefix_in_order() {
        let names = ["b.png", "a.png", "c.png", "d.png"];
        assert_eq!(search("", &names, 2), vec![0, 1]);
        assert_eq!(search("   ", &names, 10), vec![0, 1, 2, 3]);
    }

    #[test]
    fn limit_is_respected() {
        let names: Vec<String> = (0..50).map(|i| format!("photo_{i}.jpg")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        assert_eq!(search("photo", &refs, 8).len(), 8);
    }
}
