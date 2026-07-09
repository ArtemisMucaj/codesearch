use serde::{Deserialize, Serialize};

/// Category of a long-term memory item extracted from a session.
///
/// Mirrors the memory-type taxonomy used by OpenViking's session memory
/// system, reduced to the kinds that matter for a coding assistant:
///
/// - `Preference` — what the user likes/dislikes or is accustomed to
///   (code style, communication style, tooling, workflow).
/// - `Experience` — a generalizable, reusable insight distilled from a
///   session: what situation triggers it, what approach works, and why.
/// - `Skill` — reusable procedural knowledge: a repeatable flow that could
///   become an automated skill (steps, prerequisites, failure modes).
/// - `Fact` — durable declarative information worth remembering (project
///   facts, environment details, decisions and their rationale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Preference,
    Experience,
    Skill,
    Fact,
}

impl MemoryKind {
    pub const ALL: [MemoryKind; 4] = [
        MemoryKind::Preference,
        MemoryKind::Experience,
        MemoryKind::Skill,
        MemoryKind::Fact,
    ];

    /// Stable identifier used in storage and in the extraction JSON protocol.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Preference => "preference",
            MemoryKind::Experience => "experience",
            MemoryKind::Skill => "skill",
            MemoryKind::Fact => "fact",
        }
    }

    /// Plural field name used in the extraction output JSON.
    pub fn plural(&self) -> &'static str {
        match self {
            MemoryKind::Preference => "preferences",
            MemoryKind::Experience => "experiences",
            MemoryKind::Skill => "skills",
            MemoryKind::Fact => "facts",
        }
    }

    pub fn parse(s: &str) -> Option<MemoryKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "preference" | "preferences" => Some(MemoryKind::Preference),
            "experience" | "experiences" => Some(MemoryKind::Experience),
            "skill" | "skills" => Some(MemoryKind::Skill),
            "fact" | "facts" => Some(MemoryKind::Fact),
            _ => None,
        }
    }
}

impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single long-term memory item.
///
/// Items are unique per `(kind, name)`: re-extracting the same topic updates
/// the existing item (content is rewritten by the extraction model with the
/// previous content in context) rather than creating a duplicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    id: String,
    kind: MemoryKind,
    /// Short snake_case identifier for the memory topic
    /// (e.g. `rust_error_handling_style`, `duckdb_lock_conflict_fix`).
    name: String,
    /// Markdown content of the memory.
    content: String,
    /// Identifier of the session this memory was last extracted from.
    source_session_id: Option<String>,
    created_at: i64,
    updated_at: i64,
    /// Number of times this item has been re-extracted/updated.
    update_count: u32,
}

impl MemoryItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        kind: MemoryKind,
        name: String,
        content: String,
        source_session_id: Option<String>,
        created_at: i64,
        updated_at: i64,
        update_count: u32,
    ) -> Self {
        Self {
            id,
            kind,
            name,
            content,
            source_session_id,
            created_at,
            updated_at,
            update_count,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> MemoryKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn source_session_id(&self) -> Option<&str> {
        self.source_session_id.as_deref()
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }

    pub fn update_count(&self) -> u32 {
        self.update_count
    }
}

/// One message of an imported session transcript, normalized to the minimum
/// the extraction model needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    /// `user`, `assistant`, or `system`.
    pub role: String,
    /// Text content. Tool activity is summarized inline as
    /// `ToolCall: name=...; input=...` lines by the transcript parser.
    pub content: String,
    /// ISO-8601 timestamp when available.
    pub timestamp: Option<String>,
}

/// A finished session transcript, ready for memory extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTranscript {
    /// Stable session identifier (used for idempotent imports).
    pub id: String,
    /// Where the transcript came from (file path or external ID).
    pub source: String,
    pub messages: Vec<SessionMessage>,
}

impl SessionTranscript {
    /// Timestamp of the first message that carries one.
    pub fn started_at(&self) -> Option<&str> {
        self.messages.iter().find_map(|m| m.timestamp.as_deref())
    }

    /// Timestamp of the last message that carries one.
    pub fn ended_at(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find_map(|m| m.timestamp.as_deref())
    }
}

/// Record of a session that has been imported into the memory store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedSession {
    pub id: String,
    pub source: String,
    pub imported_at: i64,
    pub message_count: usize,
    /// Number of memory items written (created or updated) by the extraction.
    pub items_written: usize,
}

/// A single write/delete decided by the extraction model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOperation {
    /// Create or rewrite the item identified by `(kind, name)`.
    Upsert {
        kind: MemoryKind,
        name: String,
        content: String,
    },
    /// Remove the item identified by `(kind, name)`.
    Delete { kind: MemoryKind, name: String },
}
