//! `codesearch copilot …` — configure the GitHub Copilot chat backend.
//!
//! Three sub-flows, none of which need the (heavy) search [`Container`]; they
//! only talk to the `copilot` CLI (via [`CopilotChatClient`]) and the on-disk
//! [`CodesearchConfig`]:
//!
//! - **`login`** — verify the CLI is authenticated, then open a ratatui picker
//!   listing the account's models and save the chosen one to `config.json`.
//! - **`models`** — print the available models (table or JSON).
//! - **`status`** — print auth state and the currently-selected model.
//!
//! The GitHub OAuth device-flow is owned by the `copilot` CLI itself (it prints
//! the code/URL to the terminal and refreshes tokens automatically), so we do
//! not reimplement it. When the CLI is not yet authenticated we surface clear
//! instructions rather than fighting it for the terminal inside ratatui.

use anyhow::{Context, Result};
use github_copilot_sdk::Model;

use crate::cli::CopilotSubcommand;
use crate::connector::adapter::{CodesearchConfig, CopilotChatClient};

mod picker;

/// Entry point dispatched from the router for `codesearch copilot <sub>`.
///
/// `data_dir` is the already-tilde-expanded data directory
/// (`~/.codesearch` by default); config and CLI state live under it.
pub async fn run(subcommand: CopilotSubcommand, data_dir: &str) -> Result<String> {
    match subcommand {
        CopilotSubcommand::Login { no_pick } => login(data_dir, no_pick).await,
        CopilotSubcommand::Models { json } => models(data_dir, json).await,
        CopilotSubcommand::Status => status(data_dir).await,
    }
}

/// Build the chat client from stored config so login/models/status all share
/// the same token + `COPILOT_HOME` as ordinary chat calls.
fn client(data_dir: &str) -> Result<CopilotChatClient> {
    CopilotChatClient::from_data_dir(data_dir).context("failed to initialise Copilot client")
}

/// `copilot login`: confirm auth, then (unless `--no-pick`) run the model
/// picker and persist the selection.
async fn login(data_dir: &str, no_pick: bool) -> Result<String> {
    let client = client(data_dir)?;

    // Starting the client spawns the CLI, which triggers its own device-flow
    // login when unauthenticated (printing the code/URL to the terminal).
    let auth = client
        .auth_status()
        .await
        .context("could not reach the Copilot CLI — is `copilot` installed and on PATH?")?;

    if !auth.is_authenticated {
        return Ok(format!(
            "Not logged in to GitHub Copilot.\n\n\
             codesearch drives the official `copilot` CLI, which owns the login flow.\n\
             Authenticate it once, then re-run `codesearch copilot login`:\n\n    \
             copilot   # run it and complete the browser device-flow login\n\n\
             {}",
            auth.status_message.as_deref().unwrap_or("")
        ));
    }

    let who = auth.login.as_deref().unwrap_or("(unknown user)");

    if no_pick {
        return Ok(format!("Logged in to GitHub Copilot as {who}."));
    }

    let models = client
        .list_models()
        .await
        .context("logged in, but failed to list models")?;
    if models.is_empty() {
        return Ok(format!(
            "Logged in as {who}, but no models are available to this account."
        ));
    }

    // Pre-select whatever model is already saved, if it's still offered.
    let mut cfg = CodesearchConfig::load(data_dir)?;
    let current = cfg.copilot.as_ref().and_then(|c| c.model.clone());
    let preselected = current
        .as_deref()
        .and_then(|id| models.iter().position(|m| m.id == id))
        .unwrap_or(0);

    // The picker owns the terminal; run it on a blocking thread so it never
    // contends with the async runtime's reactor.
    let models_for_ui = models.clone();
    let choice = tokio::task::spawn_blocking(move || picker::run(&models_for_ui, preselected))
        .await
        .map_err(|e| anyhow::anyhow!("model picker task panicked: {e}"))??;

    let Some(index) = choice else {
        return Ok("Login unchanged — no model selected.".to_string());
    };
    let chosen = &models[index];

    cfg.copilot_mut().model = Some(chosen.id.clone());
    cfg.save(data_dir)?;

    Ok(format!(
        "Logged in as {who}. Selected model '{}' ({}).\n\
         Use it with: codesearch <command> --llm-target copilot",
        chosen.id, chosen.name
    ))
}

/// `copilot models`: list available models as a table or JSON.
async fn models(data_dir: &str, json: bool) -> Result<String> {
    let models = client(data_dir)?
        .list_models()
        .await
        .context("failed to list Copilot models")?;

    if json {
        return serde_json::to_string_pretty(&models).context("failed to serialize models to JSON");
    }
    Ok(render_model_table(&models))
}

/// `copilot status`: report auth state and the selected model.
async fn status(data_dir: &str) -> Result<String> {
    let cfg = CodesearchConfig::load(data_dir)?;
    let selected = cfg
        .copilot
        .as_ref()
        .and_then(|c| c.model.clone())
        .unwrap_or_else(|| "(none — CLI default)".to_string());

    let auth = client(data_dir)?.auth_status().await.ok();
    let auth_line = match auth {
        Some(a) if a.is_authenticated => {
            format!(
                "Authenticated as {}",
                a.login.as_deref().unwrap_or("(unknown)")
            )
        }
        Some(_) => "Not authenticated (run `codesearch copilot login`)".to_string(),
        None => "Copilot CLI unreachable (is `copilot` installed?)".to_string(),
    };

    Ok(format!(
        "GitHub Copilot\n  {auth_line}\n  Model: {selected}"
    ))
}

/// Render a compact table of models for the `models` / `status` output.
fn render_model_table(models: &[Model]) -> String {
    if models.is_empty() {
        return "No models available to this account.".to_string();
    }
    let id_width = models
        .iter()
        .map(|m| m.id.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());

    let mut out = format!("{:<id_width$}  {}\n", "ID", "NAME", id_width = id_width);
    for m in models {
        out.push_str(&format!(
            "{:<id_width$}  {}\n",
            m.id,
            m.name,
            id_width = id_width
        ));
    }
    out
}
