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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CodeChunk, Language, NodeType, SearchResult};

    /// Build a minimal SearchResult with a known ID; the raw score is irrelevant
    /// because rrf_fuse re-scores everything by rank position.
    fn make_result(id: &str) -> SearchResult {
        let chunk = CodeChunk::reconstitute(
            id.to_string(),
            "file.rs".to_string(),
            "fn foo() {}".to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            None,
            None,
            "repo".to_string(),
        );
        SearchResult::new(chunk, 1.0)
    }

    #[test]
    fn empty_inputs_return_empty() {
        assert!(rrf_fuse(vec![], vec![], 10).is_empty());
    }

    #[test]
    fn semantic_only_results_sorted_by_rank() {
        // Item at rank 0 (first) should receive a higher RRF score than rank 1.
        let semantic = vec![make_result("top"), make_result("bottom")];
        let fused = rrf_fuse(semantic, vec![], 10);
        assert_eq!(fused.len(), 2);
        assert!(fused[0].score() > fused[1].score());
        assert_eq!(fused[0].chunk().id(), "top");
    }

    #[test]
    fn item_in_both_lists_outranks_item_in_one_list() {
        // "shared" appears at rank 0 in both legs; "only_semantic" is in semantic only.
        // Fused score of "shared" must exceed that of "only_semantic".
        let semantic = vec![make_result("shared"), make_result("only_semantic")];
        let text = vec![make_result("shared"), make_result("only_text")];
        let fused = rrf_fuse(semantic, text, 10);
        assert_eq!(fused[0].chunk().id(), "shared");
    }

    #[test]
    fn limit_truncates_output() {
        let semantic: Vec<_> = (0..10).map(|i| make_result(&format!("item{i}"))).collect();
        let fused = rrf_fuse(semantic, vec![], 3);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn limit_zero_returns_empty() {
        let semantic = vec![make_result("x")];
        assert!(rrf_fuse(semantic, vec![], 0).is_empty());
    }

    #[test]
    fn single_item_score_matches_rrf_formula() {
        // Rank 0 → 1-based rank 1 → score = 1 / (RRF_K + 1)
        let fused = rrf_fuse(vec![make_result("x")], vec![], 10);
        let expected = 1.0 / (RRF_K + 1.0);
        assert!((fused[0].score() - expected).abs() < 1e-6);
    }

    #[test]
    fn item_in_both_lists_score_is_additive() {
        // Same item at rank 0 in both legs: score = 2 × 1/(RRF_K + 1)
        let fused = rrf_fuse(vec![make_result("x")], vec![make_result("x")], 10);
        assert_eq!(fused.len(), 1);
        let expected = 2.0 / (RRF_K + 1.0);
        assert!((fused[0].score() - expected).abs() < 1e-6);
    }

    #[test]
    fn text_only_results_sorted_by_rank() {
        let text = vec![make_result("first"), make_result("second")];
        let fused = rrf_fuse(vec![], text, 10);
        assert_eq!(fused.len(), 2);
        assert!(fused[0].score() > fused[1].score());
        assert_eq!(fused[0].chunk().id(), "first");
    }
}
