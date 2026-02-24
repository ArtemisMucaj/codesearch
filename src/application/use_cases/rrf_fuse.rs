use std::collections::HashMap;

use crate::domain::SearchResult;

/// Smoothing constant for Reciprocal Rank Fusion.
/// Higher values reduce the weight difference between high and low-ranked items.
pub const RRF_K: f32 = 60.0;

/// Score multiplier applied to results that come from test files.
/// Keeps test results discoverable while reducing their dominance over
/// production code in search rankings.
pub const TEST_FILE_PENALTY: f32 = 0.5;

/// Minimum RRF score a result must reach to be included in the output.
/// With RRF_K=60, scores range from 1/61 ≈ 0.0164 (rank 0, single list)
/// up to ~0.13 (rank 0 in all 4 variants × 2 legs).
/// This threshold drops results that appear only once and below rank ~15
/// in their list (1/76 ≈ 0.0132), keeping only well-ranked or multi-list hits.
pub const RRF_MIN_SCORE: f32 = 0.013;

/// Returns `true` if the file path looks like a test file.
///
/// Matches common conventions across languages:
/// - Directory segments: `test`, `tests`, `spec`, `specs`, `__tests__`, `__test__`, `testdata`
/// - File-name dot-components: `.test.`, `.spec.` (e.g. `foo.test.ts`, `bar.spec.js`)
/// - File-stem prefixes/suffixes: `test_foo`, `foo_test` (e.g. Python, Rust)
pub fn is_test_file(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_lowercase();
    let parts: Vec<&str> = normalized.split('/').collect();

    // Check every directory component (all but the last part, which is the filename).
    let dir_parts = if parts.len() > 1 {
        &parts[..parts.len() - 1]
    } else {
        &[][..]
    };
    for dir in dir_parts {
        if matches!(
            *dir,
            "test" | "tests" | "spec" | "specs" | "__tests__" | "__test__" | "testdata"
        ) {
            return true;
        }
    }

    // Inspect the filename itself.
    let filename = parts.last().copied().unwrap_or("");

    // Dot-separated middle components: `foo.test.ts` → middle "test" matches.
    // Skip the first (stem) and last (extension) to avoid false positives such
    // as `test.go` where "test" is the stem, not an embedded test marker.
    let dot_parts: Vec<&str> = filename.split('.').collect();
    if dot_parts.len() > 2 {
        for component in &dot_parts[1..dot_parts.len() - 1] {
            if matches!(*component, "test" | "spec") {
                return true;
            }
        }
    }

    // Stem prefix/suffix: `test_foo` or `foo_test`
    let stem = filename.split('.').next().unwrap_or("");
    if stem.starts_with("test_") || stem.ends_with("_test") {
        return true;
    }

    false
}

/// Merge ranked result lists using Reciprocal Rank Fusion.
///
/// Each result receives a score of `1 / (RRF_K + rank)` from every list it
/// appears in; scores are summed across lists.  Pass one list for a single
/// search leg, two for a hybrid semantic + text search, or N for query
/// expansion where each variant was searched independently.
///
/// Results from test files are penalised by [`TEST_FILE_PENALTY`] before the
/// final sort so that production code consistently ranks above test helpers.
pub fn rrf_fuse(lists: Vec<Vec<SearchResult>>, limit: usize) -> Vec<SearchResult> {
    let mut scores: HashMap<String, (SearchResult, f32)> = HashMap::new();

    for list in lists {
        for (rank, result) in list.into_iter().enumerate() {
            let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
            let id = result.chunk().id().to_string();
            scores
                .entry(id)
                .and_modify(|(_, s)| *s += rrf)
                .or_insert((result, rrf));
        }
    }

    let mut fused: Vec<(SearchResult, f32)> = scores.into_values().collect();

    // Apply test-file penalty before sorting so test results are ranked lower.
    for (result, score) in &mut fused {
        if is_test_file(result.chunk().file_path()) {
            *score *= TEST_FILE_PENALTY;
        }
    }

    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
        .into_iter()
        .filter(|(_, score)| *score >= RRF_MIN_SCORE)
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
        make_result_at(id, "file.rs")
    }

    fn make_result_at(id: &str, path: &str) -> SearchResult {
        let chunk = CodeChunk::reconstitute(
            id.to_string(),
            path.to_string(),
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

    // --- is_test_file ---

    #[test]
    fn non_test_files_are_not_matched() {
        assert!(!is_test_file("src/lib.rs"));
        assert!(!is_test_file("src/foo/bar.rs"));
        assert!(!is_test_file("main.py"));
        assert!(!is_test_file("src/testing_utils.rs")); // "testing_" ≠ "test_"
        assert!(!is_test_file("service/test.go")); // stem named "test" is not a dot-marker
    }

    #[test]
    fn test_directory_segments_are_matched() {
        assert!(is_test_file("tests/foo.rs"));
        assert!(is_test_file("test/foo.rs"));
        assert!(is_test_file("spec/bar.js"));
        assert!(is_test_file("specs/bar.js"));
        assert!(is_test_file("__tests__/baz.ts"));
        assert!(is_test_file("__test__/baz.ts"));
        assert!(is_test_file("testdata/fixture.json"));
        assert!(is_test_file("src/module/tests/integration.rs"));
    }

    #[test]
    fn test_dot_components_in_filename_are_matched() {
        assert!(is_test_file("src/foo.test.ts"));
        assert!(is_test_file("src/foo.spec.js"));
        assert!(is_test_file("bar.test.rs"));
    }

    #[test]
    fn test_stem_prefix_and_suffix_are_matched() {
        assert!(is_test_file("src/test_foo.py"));
        assert!(is_test_file("src/foo_test.rs"));
        assert!(is_test_file("test_bar.go"));
    }

    #[test]
    fn path_separators_are_normalised() {
        // Windows-style backslashes should work too.
        assert!(is_test_file("tests\\foo.rs"));
        assert!(is_test_file("src\\module\\tests\\bar.rs"));
    }

    // --- rrf_fuse with penalty ---

    #[test]
    fn test_file_score_is_penalised() {
        // "prod" at rank 0 scores 1/61 ≈ 0.0164 — above RRF_MIN_SCORE.
        // "test_result" at rank 0 scores 1/61 × TEST_FILE_PENALTY ≈ 0.0082 —
        // below RRF_MIN_SCORE, so it is filtered out entirely.
        let semantic = vec![make_result_at("prod", "src/lib.rs")];
        let text = vec![make_result_at("test_result", "tests/foo.rs")];
        let fused = rrf_fuse(vec![semantic, text], 10);

        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk().id(), "prod");
    }

    #[test]
    fn test_file_penalty_drops_weak_result() {
        // A single test-file result at rank 0: penalised score ≈ 0.0082 < RRF_MIN_SCORE.
        // It should be filtered out entirely.
        let fused = rrf_fuse(vec![vec![make_result_at("x", "tests/foo.rs")]], 10);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_file_in_both_lists_score_is_additive_then_penalized() {
        // Same test-file result at rank 0 in both legs:
        // raw score = 2 × 1/(RRF_K + 1), then multiplied by TEST_FILE_PENALTY.
        let fused = rrf_fuse(
            vec![
                vec![make_result_at("x", "tests/foo.rs")],
                vec![make_result_at("x", "tests/foo.rs")],
            ],
            10,
        );
        assert_eq!(fused.len(), 1);
        let expected = (2.0 / (RRF_K + 1.0)) * TEST_FILE_PENALTY;
        assert!((fused[0].score() - expected).abs() < 1e-6);
    }

    #[test]
    fn empty_inputs_return_empty() {
        assert!(rrf_fuse(vec![], 10).is_empty());
    }

    #[test]
    fn semantic_only_results_sorted_by_rank() {
        // Item at rank 0 (first) should receive a higher RRF score than rank 1.
        let semantic = vec![make_result("top"), make_result("bottom")];
        let fused = rrf_fuse(vec![semantic], 10);
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
        let fused = rrf_fuse(vec![semantic, text], 10);
        assert_eq!(fused[0].chunk().id(), "shared");
    }

    #[test]
    fn limit_truncates_output() {
        let semantic: Vec<_> = (0..10).map(|i| make_result(&format!("item{i}"))).collect();
        let fused = rrf_fuse(vec![semantic], 3);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn limit_zero_returns_empty() {
        let semantic = vec![make_result("x")];
        assert!(rrf_fuse(vec![semantic], 0).is_empty());
    }

    #[test]
    fn single_item_score_matches_rrf_formula() {
        // Rank 0 → 1-based rank 1 → score = 1 / (RRF_K + 1)
        let fused = rrf_fuse(vec![vec![make_result("x")]], 10);
        let expected = 1.0 / (RRF_K + 1.0);
        assert!((fused[0].score() - expected).abs() < 1e-6);
    }

    #[test]
    fn item_in_both_lists_score_is_additive() {
        // Same item at rank 0 in both legs: score = 2 × 1/(RRF_K + 1)
        let fused = rrf_fuse(vec![vec![make_result("x")], vec![make_result("x")]], 10);
        assert_eq!(fused.len(), 1);
        let expected = 2.0 / (RRF_K + 1.0);
        assert!((fused[0].score() - expected).abs() < 1e-6);
    }

    #[test]
    fn text_only_results_sorted_by_rank() {
        let text = vec![make_result("first"), make_result("second")];
        let fused = rrf_fuse(vec![text], 10);
        assert_eq!(fused.len(), 2);
        assert!(fused[0].score() > fused[1].score());
        assert_eq!(fused[0].chunk().id(), "first");
    }
}
