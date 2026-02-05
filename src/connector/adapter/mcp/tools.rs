use schemars::JsonSchema;
use serde::Serialize;

/// A single search result returned by the search_code tool
#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResultOutput {
    /// Path to the file containing the code
    pub file_path: String,

    /// Starting line number (1-indexed)
    pub start_line: u32,

    /// Ending line number (1-indexed)
    pub end_line: u32,

    /// Relevance score (0.0 to 1.0)
    pub score: f32,

    /// Programming language of the code
    pub language: String,

    /// Type of code construct (function, class, struct, etc.)
    pub node_type: String,

    /// Name of the symbol (function name, class name, etc.)
    pub symbol_name: Option<String>,

    /// The actual code content
    pub content: String,

    /// Repository ID this code belongs to
    pub repository_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_result_output_serialization() {
        let output = SearchResultOutput {
            file_path: "src/lib.rs".to_string(),
            start_line: 10,
            end_line: 20,
            score: 0.95,
            language: "rust".to_string(),
            node_type: "function".to_string(),
            symbol_name: Some("authenticate".to_string()),
            content: "fn authenticate() {}".to_string(),
            repository_id: "repo-123".to_string(),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("authenticate"));
        assert!(json.contains("src/lib.rs"));
    }
}
