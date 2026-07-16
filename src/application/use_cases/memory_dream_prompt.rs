//! Prompts for the dream (offline memory consolidation) use case.
//!
//! Two prompt families, both returning the same JSON operation shape:
//!
//! - **Consolidation** — one call per cluster of near-duplicate memories.
//!   The model merges overlap and, most importantly, resolves contradictions
//!   by extracting the *boundary insight* (under which conditions each side
//!   holds) instead of discarding one side.
//! - **Reflection** — one call over a compact listing of the whole store,
//!   proposing a few higher-level items (repeated experiences promoted to a
//!   skill, cross-project facts generalized to global).

use crate::domain::MemoryItem;

/// Maximum characters of a single item's content included in a consolidation
/// prompt (full content matters for contradiction detection, but a runaway
/// item must not blow the context).
const MAX_CLUSTER_ITEM_CHARS: usize = 2_000;

/// Maximum characters of a single item's content included in the reflection
/// listing (compact by design — reflection reasons over the whole store).
const MAX_REFLECTION_ITEM_CHARS: usize = 300;

/// Maximum total characters of a reflection user prompt.
const MAX_REFLECTION_PROMPT_CHARS: usize = 40_000;

/// JSON Schema for dream operations, passed to structured-output backends.
/// Kept in sync with the `DreamOutput` structs in
/// [`memory_dream`](super::memory_dream).
pub(crate) fn dream_schema() -> serde_json::Value {
    let kind = serde_json::json!({
        "type": "string",
        "enum": ["preference", "experience", "skill", "fact"]
    });
    serde_json::json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": kind,
                        "name": { "type": "string" },
                        "content": { "type": "string" },
                        "scope": { "type": ["string", "null"] }
                    },
                    "required": ["kind", "name", "content", "scope"],
                    "additionalProperties": false
                }
            },
            "delete": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": kind,
                        "name": { "type": "string" }
                    },
                    "required": ["kind", "name"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["items", "delete"],
        "additionalProperties": false
    })
}

pub(crate) fn consolidation_system_prompt() -> String {
    r#"You are consolidating an assistant's long-term memory during an offline "dream" pass.
You are given a CLUSTER of stored memory items that are semantically similar — likely duplicates, overlapping notes, or contradictory takes on the same topic, accumulated from different sessions.

Rewrite the cluster into its minimal, most useful form:

1. MERGE duplicates/overlap into ONE canonical item per real topic. Reuse the best existing name; fold every non-redundant detail in.
2. CONTRADICTIONS are the most valuable signal — do NOT simply keep the newer item. Extract the boundary insight: state both observations and the condition under which each holds (project, version, environment, situation). A resolved contradiction usually becomes a single richer item (often an `experience`).
3. DELETE items whose content is now fully covered by a merged item. You may only delete items that appear in this cluster.
4. If the items merely look similar but are genuinely about different topics, leave them alone: output empty arrays.
5. Never invent information that is not present in the items. Keep each item's markdown content self-contained.

Field notes:
- "kind": preference | experience | skill | fact (keep the most fitting kind for merged content).
- "name": short snake_case topic identifier.
- "scope": the project name the item is specific to, copied from the inputs, or null when it applies globally. When merging items with different scopes into a general insight, use null.

Output ONLY a JSON object:
{"items": [{"kind": "...", "name": "...", "content": "...", "scope": "project-or-null"}], "delete": [{"kind": "...", "name": "..."}]}"#
        .to_string()
}

pub(crate) fn consolidation_user_prompt(cluster: &[MemoryItem]) -> String {
    let mut prompt = String::from("## Cluster of similar memory items\n\n");
    for item in cluster {
        prompt.push_str(&format!(
            "### [{kind}] {name}\n- scope: {scope}\n- last updated (unix): {updated}, updates: {count}\n\n{content}\n\n",
            kind = item.kind(),
            name = item.name(),
            scope = item.scope().unwrap_or("global"),
            updated = item.updated_at(),
            count = item.update_count(),
            content = clamp(item.content(), MAX_CLUSTER_ITEM_CHARS),
        ));
    }
    prompt.push_str("Consolidate this cluster as the specified JSON object.");
    prompt
}

pub(crate) fn reflection_system_prompt(max_items: usize) -> String {
    format!(
        r#"You are reflecting over an assistant's entire long-term memory store during an offline "dream" pass, looking for higher-level insights that individual per-session extractions could not see.

Propose AT MOST {max_items} new or rewritten items, only where the evidence is strong:

1. A repeatable procedure appearing across several `experience` items → one `skill` distilling the steps, prerequisites, and failure modes.
2. The same fact or preference recorded separately under several project scopes → one global item (scope null).
3. Two items that contradict each other → one item capturing both sides and the condition under which each holds. Contradictions are the most valuable signal; never resolve one by silently ignoring a side.

Rules:
- Reuse an existing name when rewriting that topic; otherwise choose a short snake_case name.
- Do not restate single items, summarize the store, or pad the output. No evidence, no output — an empty "items" array is a good answer.
- Never invent information that is not present in the items.
- The "delete" array must be empty: reflection only writes.

Output ONLY a JSON object:
{{"items": [{{"kind": "preference|experience|skill|fact", "name": "...", "content": "...", "scope": "project-or-null"}}], "delete": []}}"#
    )
}

pub(crate) fn reflection_user_prompt(items: &[MemoryItem]) -> String {
    let mut prompt = String::from("## All stored memory items\n\n");
    for item in items {
        prompt.push_str(&format!(
            "- [{}] {} (scope: {}): {}\n",
            item.kind(),
            item.name(),
            item.scope().unwrap_or("global"),
            clamp(&one_line(item.content()), MAX_REFLECTION_ITEM_CHARS)
        ));
    }
    prompt.push_str("\nReflect over the store as the specified JSON object.");
    clamp(&prompt, MAX_REFLECTION_PROMPT_CHARS)
}

/// Format-correction retry appended after unparseable output.
pub(crate) fn format_retry_prompt() -> &'static str {
    "Your previous output could not be parsed. Output ONLY a JSON object with exactly two \
     fields: \"items\" (array of {kind, name, content, scope}) and \"delete\" (array of \
     {kind, name}). No prose, no markdown fence."
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clamp(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{truncated}...")
}
