//! Prompt construction for the claim-graph consolidation ("dream") pass.
//!
//! One LLM call per cluster of near-duplicate claims: the model reads the
//! specific claims and returns zero or more higher-level generalizations
//! (episodic → semantic abstraction, design §8.1). Kept flat and small — the
//! output is only `{statement, confidence}` per derived claim; the use case
//! attaches provenance and edges deterministically.

use crate::domain::Claim;

pub fn system_prompt() -> String {
    "You consolidate long-term memory by abstracting patterns across specific \
     claims.\n\n\
     You are given a cluster of closely-related claims. If they share a \
     generalizable pattern, return one or two higher-level claims that capture \
     it (e.g. several 'worked late before the March release' episodes → 'tends \
     to work late around releases'). Each derived claim is a short, \
     self-contained statement plus a confidence in 0..1.\n\n\
     Rules:\n\
     - Only generalize when the cluster genuinely supports it. If the claims are \
       just near-duplicates with nothing higher-level to say, return an empty \
       list.\n\
     - Do not restate a single claim verbatim; a derived claim must add \
       abstraction over the specifics.\n\
     - Keep derived confidence at or below the confidence of the specifics."
        .to_string()
}

/// Build the user prompt from a cluster of claims (their statements).
pub fn user_prompt(cluster: &[Claim]) -> String {
    let mut out = String::from("CLUSTER OF RELATED CLAIMS:\n");
    for claim in cluster {
        out.push_str(&format!("- {}\n", claim.statement));
    }
    out.push_str("\nReturn higher-level claims that generalize this cluster, or an empty list.");
    out
}

pub fn format_retry_prompt() -> String {
    "Your previous response was not valid JSON matching the required schema. \
     Reply with ONLY the JSON object, no prose or code fences."
        .to_string()
}

/// JSON Schema for the derived-claim output (mirrors `RawDream` / `RawDerived`).
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "derived": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "statement": { "type": "string" },
                        "confidence": { "type": "number" }
                    },
                    "required": ["statement", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["derived"],
        "additionalProperties": false
    })
}
