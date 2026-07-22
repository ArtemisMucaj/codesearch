//! Prompt construction for claim ingestion (experimental).
//!
//! One bounded LLM call per session: given the conversation and a handful of
//! prior claims (with ids) prefetched by semantic similarity, the model emits
//! atomic subject–predicate–object claims and, optionally, a typed relation from
//! each new claim to one of the prior ones. Kept small and flat on purpose —
//! the online tier is a small model and the Anthropic backend has no structured
//! output, so deep schemas raise the malformed-output rate (design §11).

use crate::domain::{Claim, SessionTranscript};

/// Characters of transcript text handed to the model (most recent kept).
const MAX_TRANSCRIPT_CHARS: usize = 8_000;

/// Characters of the prefetch query built from the transcript tail.
const MAX_QUERY_CHARS: usize = 800;

pub fn system_prompt() -> String {
    "You extract durable memory as atomic claims from a coding-assistant \
     conversation.\n\n\
     A claim is a single subject–predicate–object fact worth remembering across \
     sessions: a user preference, a project fact, a decision, or a reusable \
     insight. Keep each claim atomic — one fact per claim. Write `statement` as a \
     short, self-contained sentence.\n\n\
     For each claim set:\n\
     - `subject`: the entity the claim is about (usually a person, project, or \
       tool). `subject_is_entity` is almost always true.\n\
     - `predicate`: a short snake_case relation (e.g. prefers, lives_in, uses, \
       decided).\n\
     - `object`: the value. Set `object_is_entity` true when it names a distinct \
       entity (a project, tool, person), false when it is a literal value.\n\
     - `source_kind`: `user_stated` if the user asserted it directly, else \
       `assistant_inferred`.\n\
     - `confidence`: 0..1.\n\n\
     You are also given PRIOR CLAIMS with ids. If a new claim updates, refines, \
     or conflicts with a prior one, set `relation` to relate the NEW claim to \
     that prior id:\n\
     - `supersedes`: the new claim replaces an out-of-date prior claim.\n\
     - `refines`: the new claim is a more specific version of the prior one \
       (both stay true).\n\
     - `contradicts`: they genuinely conflict with no clear winner.\n\
     - `corroborates`: the new claim independently confirms the prior one.\n\
     Only set `relation` when you are confident about the target id. Omit it \
     otherwise. Return only claims that are worth remembering; return an empty \
     list if there are none."
        .to_string()
}

/// Build the user prompt: prior claims (with ids) followed by the conversation.
pub fn user_prompt(transcript: &SessionTranscript, prior: &[Claim]) -> String {
    let mut out = String::new();
    if prior.is_empty() {
        out.push_str("PRIOR CLAIMS: (none)\n\n");
    } else {
        out.push_str("PRIOR CLAIMS (id — statement):\n");
        for claim in prior {
            out.push_str(&format!("- {} — {}\n", claim.id, claim.statement));
        }
        out.push('\n');
    }
    out.push_str("CONVERSATION:\n");
    out.push_str(&render_transcript(transcript));
    out
}

/// A compact query used to prefetch prior claims by semantic similarity — the
/// tail of the conversation, where the durable facts usually land.
pub fn prefetch_query(transcript: &SessionTranscript) -> String {
    let mut parts: Vec<&str> = transcript
        .messages
        .iter()
        .rev()
        .filter(|m| m.role != "system")
        .map(|m| m.content.as_str())
        .collect();
    parts.reverse();
    let joined = parts.join(" ");
    truncate_tail(&joined, MAX_QUERY_CHARS)
}

fn render_transcript(transcript: &SessionTranscript) -> String {
    let mut lines = Vec::new();
    for m in &transcript.messages {
        if m.content.trim().is_empty() {
            continue;
        }
        lines.push(format!("{}: {}", m.role, m.content));
    }
    truncate_tail(&lines.join("\n"), MAX_TRANSCRIPT_CHARS)
}

/// Keep the last `max` chars (durable facts cluster near the end of a session).
fn truncate_tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

pub fn format_retry_prompt() -> String {
    "Your previous response was not valid JSON matching the required schema. \
     Reply with ONLY the JSON object, no prose or code fences."
        .to_string()
}

/// JSON Schema for the extraction output, kept flat for structured-output
/// backends. Mirrors the `RawIngestion` / `RawClaim` / `RawRelation` structs.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "claims": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": { "type": "string" },
                        "subject_is_entity": { "type": "boolean" },
                        "predicate": { "type": "string" },
                        "object": { "type": "string" },
                        "object_is_entity": { "type": "boolean" },
                        "statement": { "type": "string" },
                        "source_kind": {
                            "type": "string",
                            "enum": ["user_stated", "assistant_inferred"]
                        },
                        "confidence": { "type": "number" },
                        "relation": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["supersedes", "refines", "contradicts", "corroborates"]
                                },
                                "target": { "type": "string" }
                            },
                            "required": ["type", "target"],
                            "additionalProperties": false
                        }
                    },
                    "required": [
                        "subject", "subject_is_entity", "predicate", "object",
                        "object_is_entity", "statement", "source_kind", "confidence"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["claims"],
        "additionalProperties": false
    })
}
