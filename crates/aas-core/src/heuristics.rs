//! Every heuristic of the engine lives here (see CLAUDE.md, "ヒューリスティック方針", and
//! docs/design.md §14).

use aas_protocol::SearchResult;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// **H1** — ranks candidate paths for an `@` mention query.
///
/// * Estimates: which files the user most likely means by the typed fragment.
/// * Basis: nucleo's fuzzy score with path-aware bonuses (the matcher used by Helix);
///   ties are broken by shorter path, then lexicographically, so the order is reproducible.
/// * If wrong: candidates appear in a less useful order; the user still picks explicitly, so
///   no action is taken on the guess.
pub fn heuristic_rank_file_matches(
    candidates: &[(String, bool)],
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    tracing::debug!(
        heuristic = "H1",
        query,
        candidates = candidates.len(),
        limit,
        "ranking file candidates"
    );
    let query = query.trim();
    if query.is_empty() {
        let mut all: Vec<&(String, bool)> = candidates.iter().collect();
        all.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
        return all
            .into_iter()
            .take(limit)
            .map(|(p, d)| SearchResult {
                path: p.clone(),
                is_dir: *d,
            })
            .collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored = pattern.match_list(
        candidates.iter().map(|(p, d)| Candidate(p.as_str(), *d)),
        &mut matcher,
    );
    scored.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa)
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then_with(|| a.0.cmp(b.0))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(c, _)| SearchResult {
            path: c.0.to_owned(),
            is_dir: c.1,
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Candidate<'a>(&'a str, bool);

impl AsRef<str> for Candidate<'_> {
    fn as_ref(&self) -> &str {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(paths: &[&str]) -> Vec<(String, bool)> {
        paths.iter().map(|p| (p.to_string(), false)).collect()
    }

    #[test]
    fn ranks_better_matches_first_and_is_deterministic() {
        let cands = c(&[
            "docs/reconnect.md",
            "crates/aas-server/tests/reconnect.rs",
            "src/main.rs",
            "README.md",
        ]);
        let r = heuristic_rank_file_matches(&cands, "reconn", 10);
        assert!(r.len() >= 2);
        assert!(r.iter().all(|x| x.path.contains("reconnect")));
        assert_eq!(r, heuristic_rank_file_matches(&cands, "reconn", 10));
        let empty = heuristic_rank_file_matches(&cands, "", 2);
        assert_eq!(
            empty.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
            vec!["README.md", "src/main.rs"]
        );
    }
}
