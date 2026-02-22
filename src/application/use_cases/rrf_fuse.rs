use std::collections::HashMap;

use crate::domain::SearchResult;

/// Smoothing constant for Reciprocal Rank Fusion.
/// Higher values reduce the weight difference between high and low-ranked items.
pub const RRF_K: f32 = 60.0;

/// Merge two ranked result lists using Reciprocal Rank Fusion.
///
/// Each result receives a score of `1 / (RRF_K + rank)` from each list it
/// appears in.  The scores are summed, and the top `limit` results by fused
/// score are returned.
pub fn rrf_fuse(
    semantic: Vec<SearchResult>,
    text: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    let mut scores: HashMap<String, (SearchResult, f32)> = HashMap::new();

    for (rank, result) in semantic.into_iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        let id = result.chunk().id().to_string();
        scores
            .entry(id)
            .and_modify(|(_, s)| *s += rrf)
            .or_insert((result, rrf));
    }
    for (rank, result) in text.into_iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        let id = result.chunk().id().to_string();
        scores
            .entry(id)
            .and_modify(|(_, s)| *s += rrf)
            .or_insert((result, rrf));
    }

    let mut fused: Vec<(SearchResult, f32)> = scores.into_values().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
        .into_iter()
        .take(limit)
        .map(|(r, score)| SearchResult::new(r.chunk().clone(), score))
        .collect()
}
