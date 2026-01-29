use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Represents an indexed code repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub chunk_count: u64,
    pub file_count: u64,
}

impl Repository {
    pub fn new(name: String, path: String) -> Self {
        let now = chrono_timestamp();
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            path,
            created_at: now,
            updated_at: now,
            chunk_count: 0,
            file_count: 0,
        }
    }

    pub fn update_stats(&mut self, chunk_count: u64, file_count: u64) {
        self.chunk_count = chunk_count;
        self.file_count = file_count;
        self.updated_at = chrono_timestamp();
    }
}

fn chrono_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}
