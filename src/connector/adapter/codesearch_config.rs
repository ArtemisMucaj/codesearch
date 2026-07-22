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
    /// The active LLM backend for `serve` mode, persisted so a chosen backend
    /// (e.g. `"copilot"`) survives restarts and can be switched at runtime via
    /// the management API. Absent means "use the boot default" (the
    /// `--llm-target` flag, else the built-in default). Values match
    /// [`LlmTarget`]'s string form: `"open-ai"`, `"anthropic"`, `"copilot"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_target: Option<String>,

    /// GitHub Copilot chat backend configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot: Option<CopilotConfig>,

    /// Named OpenAI-compatible endpoints (LM Studio, vLLM, hosted OpenAI, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAiConfig>,

    /// Long-term memory / dream-cycle configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,
}

/// Configuration for the memory dream scheduler run by `codesearch serve`.
///
/// Every field is optional so a hand-edited partial section round-trips; the
/// accessor methods apply the defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Master switch for scheduled dreaming in serve mode (default `true`).
    /// `codesearch memory dream` always works regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dream_enabled: Option<bool>,

    /// Hours between full dream cycles in serve mode (default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dream_interval_hours: Option<u64>,

    /// Minutes a session must be inactive before it counts as finished and is
    /// harvested (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle_minutes: Option<u64>,

    /// Whether serve mode automatically imports finished sessions between
    /// dream cycles (default `true`). Each import spends LLM extraction calls,
    /// so users on paid endpoints may want this off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_import: Option<bool>,
}

impl MemoryConfig {
    pub const DEFAULT_DREAM_INTERVAL_HOURS: u64 = 4;
    pub const DEFAULT_SESSION_IDLE_MINUTES: u64 = 60;

    pub fn dream_enabled(&self) -> bool {
        self.dream_enabled.unwrap_or(true)
    }

    pub fn dream_interval_hours(&self) -> u64 {
        self.dream_interval_hours
            .filter(|h| *h > 0)
            .unwrap_or(Self::DEFAULT_DREAM_INTERVAL_HOURS)
    }

    pub fn session_idle_minutes(&self) -> u64 {
        self.session_idle_minutes
            .filter(|m| *m > 0)
            .unwrap_or(Self::DEFAULT_SESSION_IDLE_MINUTES)
    }

    pub fn auto_import(&self) -> bool {
        self.auto_import.unwrap_or(true)
    }
}

/// A set of named OpenAI-compatible endpoints plus which one is active.
///
/// Lets a user (or a native app via the management API) register several
/// servers and switch between them at runtime, instead of the single
/// env-var-configured endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAiConfig {
    /// Name of the endpoint used when `--llm-target open-ai` is selected with no
    /// explicit `--openai-endpoint`. When unset (or naming a missing endpoint),
    /// callers fall back to the `OPENAI_*` environment variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,

    /// Registered endpoints, keyed by a user-chosen name (e.g. `"lmstudio"`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub endpoints: std::collections::BTreeMap<String, OpenAiEndpoint>,
}

/// One OpenAI-compatible server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAiEndpoint {
    /// Base URL, e.g. `http://localhost:1234` (no `/v1` suffix).
    pub base_url: String,

    /// Model id sent in chat requests. When absent, the server's default (or a
    /// built-in default) is used; run the picker to select one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Bearer API key for hosted servers. Absent/empty for local servers like
    /// LM Studio. Never returned over the management API (masked on read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Persisted GitHub Copilot settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CopilotConfig {
    /// GitHub OAuth token (`ghu_…`) captured during `copilot login`. Sent as a
    /// `Bearer` credential on every Copilot API request. When absent, requests
    /// are unauthenticated and fail — run `copilot login` first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Model id selected in the picker (e.g. `"claude-sonnet-4.5"`). When
    /// absent the Copilot API's default model is used.
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

    /// Mutable access to the OpenAI section, creating it if absent.
    pub fn openai_mut(&mut self) -> &mut OpenAiConfig {
        self.openai.get_or_insert_with(OpenAiConfig::default)
    }

    /// Resolve which named OpenAI endpoint to use, honoring an explicit
    /// `name_override` first, then the configured `active` endpoint. Returns
    /// `None` when neither is set (or names a missing endpoint) so the caller
    /// falls back to the `OPENAI_*` environment variables.
    pub fn resolve_openai_endpoint(&self, name_override: Option<&str>) -> Option<OpenAiEndpoint> {
        let openai = self.openai.as_ref()?;
        let name = name_override.or(openai.active.as_deref())?;
        openai.endpoints.get(name).cloned()
    }

    /// The persisted active LLM target, if any. An unparseable stored value is
    /// treated as absent (rather than an error) so a hand-edited typo falls back
    /// to the boot default instead of wedging the server.
    pub fn active_llm_target(&self) -> Option<crate::cli::LlmTarget> {
        self.llm_target.as_deref()?.parse().ok()
    }

    /// Set the persisted active LLM target.
    pub fn set_llm_target(&mut self, target: crate::cli::LlmTarget) {
        self.llm_target = Some(target.as_str().to_string());
    }

    /// Async wrapper around [`Self::load`] that runs the (blocking) file read on
    /// a blocking thread — for use from async request handlers that must not
    /// stall the executor.
    pub async fn load_async(data_dir: &str) -> Result<Self, DomainError> {
        let data_dir = data_dir.to_string();
        tokio::task::spawn_blocking(move || Self::load(&data_dir))
            .await
            .map_err(|e| DomainError::internal(format!("config load task panicked: {e}")))?
    }

    /// Async wrapper around [`Self::save`] (see [`Self::load_async`]).
    pub async fn save_async(self, data_dir: &str) -> Result<(), DomainError> {
        let data_dir = data_dir.to_string();
        tokio::task::spawn_blocking(move || self.save(&data_dir))
            .await
            .map_err(|e| DomainError::internal(format!("config save task panicked: {e}")))?
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
    fn llm_target_round_trips_and_parses() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let mut cfg = CodesearchConfig::default();
        assert_eq!(cfg.active_llm_target(), None, "absent by default");
        cfg.set_llm_target(crate::cli::LlmTarget::Copilot);
        cfg.save(data_dir).unwrap();

        let loaded = CodesearchConfig::load(data_dir).unwrap();
        assert_eq!(loaded.llm_target.as_deref(), Some("copilot"));
        assert_eq!(
            loaded.active_llm_target(),
            Some(crate::cli::LlmTarget::Copilot)
        );
    }

    #[test]
    fn unparseable_llm_target_is_treated_as_absent() {
        // A hand-edited typo must fall back to the boot default, not wedge boot.
        let cfg = CodesearchConfig {
            llm_target: Some("nonsense".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.active_llm_target(), None);
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

    #[test]
    fn openai_endpoints_round_trip() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let mut cfg = CodesearchConfig::default();
        let openai = cfg.openai_mut();
        openai.endpoints.insert(
            "lmstudio".to_string(),
            OpenAiEndpoint {
                base_url: "http://localhost:1234".to_string(),
                model: Some("gemma".to_string()),
                api_key: None,
            },
        );
        openai.active = Some("lmstudio".to_string());
        cfg.save(data_dir).unwrap();

        let loaded = CodesearchConfig::load(data_dir).unwrap();
        let openai = loaded.openai.expect("openai section present");
        assert_eq!(openai.active.as_deref(), Some("lmstudio"));
        let ep = openai.endpoints.get("lmstudio").expect("endpoint present");
        assert_eq!(ep.base_url, "http://localhost:1234");
        assert_eq!(ep.model.as_deref(), Some("gemma"));
    }

    #[test]
    fn resolve_openai_endpoint_precedence() {
        let mut cfg = CodesearchConfig::default();
        let openai = cfg.openai_mut();
        openai.endpoints.insert(
            "a".to_string(),
            OpenAiEndpoint {
                base_url: "http://a".to_string(),
                ..Default::default()
            },
        );
        openai.endpoints.insert(
            "b".to_string(),
            OpenAiEndpoint {
                base_url: "http://b".to_string(),
                ..Default::default()
            },
        );
        openai.active = Some("a".to_string());

        // Explicit override wins over active.
        assert_eq!(
            cfg.resolve_openai_endpoint(Some("b")).unwrap().base_url,
            "http://b"
        );
        // Falls back to active when no override.
        assert_eq!(
            cfg.resolve_openai_endpoint(None).unwrap().base_url,
            "http://a"
        );
        // A missing name resolves to None (caller falls back to env).
        assert!(cfg.resolve_openai_endpoint(Some("missing")).is_none());
    }

    #[test]
    fn resolve_openai_endpoint_none_without_config() {
        let cfg = CodesearchConfig::default();
        assert!(cfg.resolve_openai_endpoint(None).is_none());
    }
}
