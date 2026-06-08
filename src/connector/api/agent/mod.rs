//! Agent integration: wiring codesearch into AI coding assistants so they reach
//! for semantic search instead of grepping.
//!
//! Two layers, mirroring how graphify drives its adoption:
//!
//! * **Agent hooks** ([`install`]/[`uninstall`]) — per-platform integrations
//!   that nudge the assistant toward `codesearch search`/`context`/`impact`
//!   whenever it is about to grep or read source files. Claude Code and OpenCode
//!   get a live, reactive nudge; Pi gets a live TypeScript extension; Zed (which
//!   has no payload pre-tool hook) gets always-on `.rules` plus MCP registration.
//! * **Git hooks** ([`git_hooks`]) — `post-commit`/`post-checkout` automation
//!   that keeps the index fresh so the nudge always points at current data.
//!
//! All decision logic for the live nudge lives in [`pre_tool_call`] and is
//! reused across platforms via the `codesearch pre-tool-call` command.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::cli::AgentPlatform;
use crate::Commands;

pub mod claude;
pub mod git_hooks;
pub mod marker;
pub mod opencode;
pub mod pi;
pub mod pre_tool_call;
pub mod zed;

/// Whether an integration is written to the user profile or the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Installed once into the user's home profile; applies to every repository.
    Global,
    /// Installed into the current working directory; applies to this repo only.
    Project,
}

impl Scope {
    fn from_project_flag(project: bool) -> Self {
        if project {
            Scope::Project
        } else {
            Scope::Global
        }
    }
}

/// Route the install-related CLI commands. Returns the user-facing report.
pub fn dispatch(command: &Commands) -> Result<String> {
    match command {
        Commands::Install { platform, project } => {
            install(*platform, Scope::from_project_flag(*project))
        }
        Commands::Uninstall { platform, project } => {
            uninstall(*platform, Scope::from_project_flag(*project))
        }
        Commands::Hooks { subcommand } => git_hooks::dispatch(subcommand),
        // The router/main only dispatches the variants above to this module.
        _ => unreachable!("agent::dispatch called with a non-agent command"),
    }
}

/// Install the integration for one platform (or all of them).
pub fn install(platform: AgentPlatform, scope: Scope) -> Result<String> {
    run_each(platform, scope, |p, s| match p {
        AgentPlatform::Claude => claude::install(s),
        AgentPlatform::Opencode => opencode::install(s),
        AgentPlatform::Pi => pi::install(s),
        AgentPlatform::Zed => zed::install(s),
        AgentPlatform::All => unreachable!(),
    })
}

/// Remove the integration for one platform (or all of them).
pub fn uninstall(platform: AgentPlatform, scope: Scope) -> Result<String> {
    run_each(platform, scope, |p, s| match p {
        AgentPlatform::Claude => claude::uninstall(s),
        AgentPlatform::Opencode => opencode::uninstall(s),
        AgentPlatform::Pi => pi::uninstall(s),
        AgentPlatform::Zed => zed::uninstall(s),
        AgentPlatform::All => unreachable!(),
    })
}

/// Expand `All` and run `f` for each concrete platform, joining the reports.
fn run_each(
    platform: AgentPlatform,
    scope: Scope,
    f: impl Fn(AgentPlatform, Scope) -> Result<Vec<String>>,
) -> Result<String> {
    let platforms: Vec<AgentPlatform> = match platform {
        AgentPlatform::All => vec![
            AgentPlatform::Claude,
            AgentPlatform::Opencode,
            AgentPlatform::Pi,
            AgentPlatform::Zed,
        ],
        single => vec![single],
    };

    let mut sections = Vec::new();
    for p in platforms {
        let lines = f(p, scope)?;
        let mut section = format!("{}:", platform_label(p));
        for line in lines {
            section.push_str(&format!("\n  {line}"));
        }
        sections.push(section);
    }
    Ok(sections.join("\n\n"))
}

fn platform_label(p: AgentPlatform) -> &'static str {
    match p {
        AgentPlatform::Claude => "Claude Code",
        AgentPlatform::Opencode => "OpenCode",
        AgentPlatform::Pi => "Pi",
        AgentPlatform::Zed => "Zed",
        AgentPlatform::All => "all",
    }
}

// ---------------------------------------------------------------------------
// Shared filesystem / JSON helpers used by the per-platform installers.
// ---------------------------------------------------------------------------

/// The command the installed hooks invoke. We assume `codesearch` is on PATH —
/// the same assumption the bundled skill already makes.
pub const PRE_TOOL_CALL_COMMAND: &str = "codesearch pre-tool-call";

/// Resolve the user's home directory from `$HOME`.
pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .context("cannot determine home directory: $HOME is not set")
}

/// The base directory for a scope: home for [`Scope::Global`], cwd for
/// [`Scope::Project`].
pub fn scope_root(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Global => home_dir(),
        Scope::Project => std::env::current_dir().context("cannot determine current directory"),
    }
}

/// Render a path for display, collapsing the home prefix to `~`.
pub fn display_path(path: &Path) -> String {
    if let Ok(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Load a JSON object from `path`, returning an empty map when the file is
/// missing or cannot be parsed as an object. (We never silently discard a
/// parseable object; callers merge into what we return.)
pub fn load_json_object(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => Map::new(),
    }
}

/// Write a JSON value to `path` as pretty-printed JSON, creating parent dirs.
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Write `contents` to `path` (creating parent dirs), returning whether the file
/// changed. Used for the JS/TS/rules files that have fixed content.
pub fn write_file_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}
