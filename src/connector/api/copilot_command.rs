//! `codesearch copilot …` — configure the GitHub Copilot chat backend.
//!
//! Three sub-flows, none of which need the (heavy) search [`Container`]; they
//! only talk to GitHub / the Copilot API over HTTP and the on-disk
//! [`CodesearchConfig`]:
//!
//! - **`login`** — run the GitHub OAuth device flow (print the code + URL, poll
//!   for the token), store the token in `config.json`, then open a ratatui
//!   picker listing the account's models and save the chosen one.
//! - **`models`** — print the available models (table or JSON).
//! - **`status`** — print auth state and the currently-selected model.
//!
//! codesearch performs the device flow itself (see [`copilot_auth`]) and calls
//! the Copilot API directly, so there is no external CLI dependency.

use anyhow::{Context, Result};

use crate::cli::CopilotSubcommand;
use crate::connector::adapter::{copilot_auth, CodesearchConfig, CopilotChatClient, CopilotModel};

mod picker;

/// Entry point dispatched from the router for `codesearch copilot <sub>`.
///
/// `data_dir` is the already-tilde-expanded data directory
/// (`~/.codesearch` by default); the config file lives under it.
pub async fn run(subcommand: CopilotSubcommand, data_dir: &str) -> Result<String> {
    match subcommand {
        CopilotSubcommand::Login { no_pick } => login(data_dir, no_pick).await,
        CopilotSubcommand::Models { json } => models(data_dir, json).await,
        CopilotSubcommand::Status => status(data_dir).await,
    }
}

/// `copilot login`: run the device flow, store the token, then (unless
/// `--no-pick`) run the model picker and persist the selection.
async fn login(data_dir: &str, no_pick: bool) -> Result<String> {
    let http = reqwest::Client::new();

    // Step 1: get a device code and show it to the user.
    let device = copilot_auth::request_device_code(&http)
        .await
        .context("failed to start GitHub device-flow login")?;

    println!(
        "To authorize codesearch with GitHub Copilot:\n\n  \
         1. Open {}\n  2. Enter the code: {}\n\nWaiting for authorization…",
        device.verification_uri(),
        device.user_code()
    );

    // Step 2: poll until the user completes the browser step.
    let token = copilot_auth::poll_for_token(&http, &device)
        .await
        .context("GitHub device-flow login failed")?;

    // Persist the token immediately so a later picker failure doesn't lose it.
    let mut cfg = CodesearchConfig::load(data_dir)?;
    cfg.copilot_mut().github_token = Some(token);
    cfg.save(data_dir)?;

    if no_pick {
        return Ok("Logged in to GitHub Copilot. Token saved.".to_string());
    }

    // Step 3: list models and let the user pick one.
    let client = CopilotChatClient::from_data_dir(data_dir)
        .context("failed to initialise Copilot client after login")?;
    let models = client
        .list_models()
        .await
        .context("logged in, but failed to list models")?;
    if models.is_empty() {
        return Ok("Logged in, but no models are available to this account.".to_string());
    }

    // Pre-select whatever model is already saved, if it's still offered.
    let current = cfg.copilot.as_ref().and_then(|c| c.model.clone());
    let preselected = current
        .as_deref()
        .and_then(|id| models.iter().position(|m| m.id == id))
        .unwrap_or(0);

    // The picker owns the terminal; run it on a blocking thread so it never
    // contends with the async runtime's reactor. `models` is moved in and the
    // chosen entry is returned back out, so nothing is cloned.
    let chosen = tokio::task::spawn_blocking(move || {
        picker::run(&models, preselected).map(|choice| choice.map(|index| models[index].clone()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("model picker task panicked: {e}"))??;

    let Some(chosen) = chosen else {
        return Ok("Login saved — no model selected (using the default).".to_string());
    };

    cfg.copilot_mut().model = Some(chosen.id.clone());
    cfg.save(data_dir)?;

    Ok(format!(
        "Logged in to GitHub Copilot. Selected model '{}' ({}).\n\
         Use it with: codesearch <command> --llm-target copilot",
        chosen.id, chosen.name
    ))
}

/// `copilot models`: list available models as a table or JSON.
async fn models(data_dir: &str, json: bool) -> Result<String> {
    let client = CopilotChatClient::from_data_dir(data_dir)
        .context("failed to initialise Copilot client")?;
    let models = client
        .list_models()
        .await
        .context("failed to list Copilot models")?;

    if json {
        return serde_json::to_string_pretty(&models).context("failed to serialize models to JSON");
    }
    Ok(render_model_table(&models))
}

/// `copilot status`: report whether a token is stored and the selected model.
async fn status(data_dir: &str) -> Result<String> {
    let copilot = CodesearchConfig::load_copilot(data_dir)?;
    let auth_line = if copilot
        .github_token
        .as_deref()
        .is_some_and(|t| !t.is_empty())
    {
        "Token: stored (run `codesearch copilot login` to refresh)"
    } else {
        "Token: none (run `codesearch copilot login`)"
    };
    let model = copilot
        .model
        .unwrap_or_else(|| "(none — API default)".to_string());

    Ok(format!("GitHub Copilot\n  {auth_line}\n  Model: {model}"))
}

/// Render a compact table of models for the `models` output.
fn render_model_table(models: &[CopilotModel]) -> String {
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
