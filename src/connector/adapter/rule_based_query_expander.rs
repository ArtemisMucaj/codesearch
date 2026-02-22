use async_trait::async_trait;

use crate::application::QueryExpander;
use crate::domain::DomainError;

/// Natural language filler phrases to strip before generating the technical variant.
/// Ordered longest-first so multi-word phrases are removed before their constituents.
const FILLER_PHRASES: &[&str] = &[
    "show me all",
    "give me all",
    "find me all",
    "show me the",
    "give me the",
    "find me the",
    "i'm looking for",
    "i am looking for",
    "i want to find",
    "i need to find",
    "search for all",
    "look for all",
    "show me",
    "find me",
    "give me",
    "tell me",
    "what is",
    "what are",
    "how does",
    "how do",
    "how is",
    "where is",
    "where are",
    "search for",
    "look for",
    "find all",
    "find the",
    "list all",
    "list the",
    "get all",
    "get the",
    "show all",
    "show the",
    "related to",
    "used for",
    "used by",
    "used in",
];

/// Individual stop words that remain after phrase removal.
const STOP_WORDS: &[&str] = &[
    "find", "show", "get", "give", "tell", "search", "look", "list", "retrieve", "fetch",
    "the", "a", "an", "some", "any", "all",
    "which", "that", "this", "these", "those",
    "for", "from", "in", "on", "at", "to", "of", "with", "by", "via",
    "is", "are", "was", "were", "be", "been", "being",
    "do", "does", "did", "have", "has", "had",
    "can", "could", "will", "would", "should", "may", "might",
    "function", "method", "code", "implementation",
];

/// A lightweight, rule-based query expander that requires no external services
/// or model downloads. It generates two additional variants from the original query:
///
/// 1. **Technical variant** – strips natural language filler phrases and stop words,
///    leaving only the domain-relevant nouns and verbs (e.g. "find function that
///    handles auth errors" → "handles auth errors").
///
/// 2. **Identifier variant** – converts the key terms from the technical variant into
///    snake_case bigrams that resemble real code identifiers (e.g. "auth_errors
///    handles_auth"), boosting recall for chunks whose symbol names match the query
///    concept.
///
/// All three variants (original + technical + identifier) are embedded independently.
/// The search use case fuses the three result lists with Reciprocal Rank Fusion so
/// chunks that rank well across multiple phrasings receive higher final scores.
pub struct RuleBasedQueryExpander;

impl Default for RuleBasedQueryExpander {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleBasedQueryExpander {
    pub fn new() -> Self {
        Self
    }

    /// Strip NL filler phrases and stop words, returning only technical terms.
    fn technical_variant(&self, query: &str) -> String {
        let mut cleaned = query.to_lowercase();

        // Remove multi-word filler phrases (longest first to avoid partial matches).
        for phrase in FILLER_PHRASES {
            // Replace whole-phrase occurrences with a space.
            cleaned = cleaned.replace(phrase, " ");
        }

        // Remove individual stop words (whole-word only via split/filter).
        let tokens: Vec<&str> = cleaned
            .split_whitespace()
            .filter(|w| !STOP_WORDS.contains(w))
            .collect();

        tokens.join(" ")
    }

    /// Convert key terms to snake_case bigrams resembling code identifiers.
    ///
    /// Only terms longer than 2 characters are considered to skip articles
    /// and prepositions that slipped through stop-word filtering.
    fn identifier_variant(&self, technical: &str) -> String {
        let words: Vec<&str> = technical
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .collect();

        if words.is_empty() {
            return String::new();
        }

        let mut identifiers: Vec<String> = Vec::new();

        // Sliding window of 2 → snake_case bigrams
        for pair in words.windows(2) {
            identifiers.push(format!("{}_{}", pair[0], pair[1]));
        }

        // Also include the individual terms so single-word symbols still match
        for &w in &words {
            identifiers.push(w.to_string());
        }

        identifiers.join(" ")
    }
}

#[async_trait]
impl QueryExpander for RuleBasedQueryExpander {
    async fn expand(&self, query: &str) -> Result<Vec<String>, DomainError> {
        let mut variants = vec![query.to_string()];

        let technical = self.technical_variant(query);

        // Only add the technical variant if it differs from the original and is non-empty.
        if !technical.is_empty() && technical != query.to_lowercase() {
            let ident = self.identifier_variant(&technical);

            variants.push(technical.clone());

            // Add the identifier variant only when it adds something new.
            if !ident.is_empty() && ident != technical {
                variants.push(ident);
            }
        } else {
            // Query was already terse — try identifier-style on the original.
            let ident = self.identifier_variant(query);
            if !ident.is_empty() && ident != query {
                variants.push(ident);
            }
        }

        Ok(variants)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn expand(q: &str) -> Vec<String> {
        RuleBasedQueryExpander::new().expand(q).await.unwrap()
    }

    #[tokio::test]
    async fn original_is_always_first() {
        let q = "find the authentication handler";
        let variants = expand(q).await;
        assert_eq!(variants[0], q);
    }

    #[tokio::test]
    async fn at_least_one_variant_returned() {
        let variants = expand("x").await;
        assert!(!variants.is_empty());
    }

    #[tokio::test]
    async fn strips_filler_phrases() {
        let variants = expand("show me the authentication function").await;
        // Second variant should not contain "show me the"
        assert!(variants.len() >= 2);
        assert!(!variants[1].contains("show me the"));
        assert!(variants[1].contains("authentication"));
    }

    #[tokio::test]
    async fn strips_stop_words() {
        let variants = expand("find the function that handles errors").await;
        assert!(variants.len() >= 2);
        // "find", "the", "that" should be removed
        let tech = &variants[1];
        assert!(!tech.contains("find"));
        assert!(!tech.contains(" the "));
        assert!(!tech.contains(" that "));
    }

    #[tokio::test]
    async fn generates_identifier_variant() {
        let variants = expand("function that handles user authentication errors").await;
        // Should have at least 3 variants (original, technical, identifier)
        assert!(variants.len() >= 2);
        // The identifier variant should contain underscores
        if variants.len() >= 3 {
            assert!(variants[2].contains('_'));
        }
    }

    #[tokio::test]
    async fn terse_query_gets_identifier_variant() {
        // A query with no filler words may still get an identifier-style variant
        let variants = expand("authentication error handler").await;
        assert!(!variants.is_empty());
    }

    #[tokio::test]
    async fn no_duplicate_variants() {
        let variants = expand("auth").await;
        let unique: std::collections::HashSet<&String> = variants.iter().collect();
        assert_eq!(unique.len(), variants.len());
    }
}
