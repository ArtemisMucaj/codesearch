//! On-disk user configuration stored at `<data_dir>/config.json`
//! (default `~/.codesearch/config.json`).
//!
//! Today this holds the GitHub Copilot chat backend's persisted state — the
//! OAuth token captured at login and the model chosen in the picker — so that
//! `codesearch <cmd> --llm-target copilot` and the `serve` HTTP API can drive
//! Copilot without re-prompting. The structure is intentionally open for future
//! sections (other providers, defaults) without a schema migration: unknown
//! fields are ignored on read and omitted-when-empty on write.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::DomainError;

/// File name (under the resolved data directory) holding user configuration.
const CONFIG_FILE: &str = "config.json";

/// Root configuration document persisted to `<data_dir>/config.json`.
///
/// Every section is optional so a partially-written file (e.g. only a Copilot
/// token, no model yet) round-trips cleanly, and adding a new section never
/// invalidates an existing file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodesearchConfig {
    /// GitHub Copilot chat backend configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot: Option<CopilotConfig>,
}

/// Persisted GitHub Copilot settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CopilotConfig {
    /// GitHub OAuth token (`ghu_…`) captured during `copilot login`. When
    /// present it is handed to the Copilot CLI so it skips its own interactive
    /// login; when absent the CLI performs the device-flow login itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Model id selected in the picker (e.g. `"claude-sonnet-4.5"`). When
    /// absent the CLI's default model is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl CodesearchConfig {
    /// Absolute path of the config file inside `data_dir`.
    pub fn path_in(data_dir: &str) -> PathBuf {
        Path::new(data_dir).join(CONFIG_FILE)
    }

    /// Load configuration from `<data_dir>/config.json`.
    ///
    /// A missing file is not an error — it yields [`CodesearchConfig::default`]
    /// so first-run flows work without a pre-existing file. A present-but-
    /// malformed file *is* an error, so we never silently discard a user's
    /// saved token by treating corruption as "empty".
    pub fn load(data_dir: &str) -> Result<Self, DomainError> {
        let path = Self::path_in(data_dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(DomainError::internal(format!(
                    "failed to read {}: {e}",
                    path.display()
                )))
            }
        };
        serde_json::from_str(&contents)
            .map_err(|e| DomainError::internal(format!("failed to parse {}: {e}", path.display())))
    }

    /// Persist configuration to `<data_dir>/config.json`, creating the
    /// directory if needed. Written pretty-printed so users can hand-edit it.
    pub fn save(&self, data_dir: &str) -> Result<(), DomainError> {
        std::fs::create_dir_all(data_dir).map_err(|e| {
            DomainError::internal(format!("failed to create data dir {data_dir}: {e}"))
        })?;
        let path = Self::path_in(data_dir);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| DomainError::internal(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, json).map_err(|e| {
            DomainError::internal(format!("failed to write {}: {e}", path.display()))
        })?;

        // The file can hold a GitHub OAuth token, so keep it owner-only rather
        // than inheriting the (often world/group-readable) umask default.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |e| {
                    DomainError::internal(format!(
                        "failed to restrict permissions on {}: {e}",
                        path.display()
                    ))
                },
            )?;
        }
        Ok(())
    }

    /// Mutable access to the Copilot section, creating it if absent.
    pub fn copilot_mut(&mut self) -> &mut CopilotConfig {
        self.copilot.get_or_insert_with(CopilotConfig::default)
    }

    /// Load the config from `data_dir` and take its Copilot section (or the
    /// default when absent) in one step — the common read path for callers that
    /// only care about the Copilot settings.
    pub fn load_copilot(data_dir: &str) -> Result<CopilotConfig, DomainError> {
        Ok(Self::load(data_dir)?.copilot.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let cfg = CodesearchConfig::load(dir.path().to_str().unwrap()).unwrap();
        assert!(cfg.copilot.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let mut cfg = CodesearchConfig::default();
        cfg.copilot_mut().github_token = Some("ghu_example".to_string());
        cfg.copilot_mut().model = Some("claude-sonnet-4.5".to_string());
        cfg.save(data_dir).unwrap();

        let loaded = CodesearchConfig::load(data_dir).unwrap();
        let copilot = loaded.copilot.expect("copilot section present");
        assert_eq!(copilot.github_token.as_deref(), Some("ghu_example"));
        assert_eq!(copilot.model.as_deref(), Some("claude-sonnet-4.5"));
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        std::fs::write(CodesearchConfig::path_in(data_dir), "{ not json").unwrap();
        assert!(CodesearchConfig::load(data_dir).is_err());
    }

    #[test]
    fn empty_copilot_section_is_omitted_from_output() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        CodesearchConfig::default().save(data_dir).unwrap();
        let written = std::fs::read_to_string(CodesearchConfig::path_in(data_dir)).unwrap();
        assert!(!written.contains("copilot"), "got: {written}");
    }
}
