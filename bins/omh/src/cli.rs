use std::env;

use anyhow::{Context, Result, bail};
use runtime::AgentRuntime;

use crate::{auth, init_harness};

pub async fn run_cli(prompt: &str, agent: &str, continue_last: bool) -> Result<()> {
    let mut harness = init_harness()?;
    register_providers_from_env(&mut harness)?;

    let workspace_root = std::env::current_dir()?;
    harness.connect_mcp_servers(&workspace_root);

    let agent_def = harness
        .agent_registry
        .get(agent)
        .with_context(|| format!("unknown agent: {agent}"))?;

    if harness.provider_registry.list().is_empty() {
        bail!(
            "no provider configured. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GITHUB_COPILOT_TOKEN, or run 'omh auth login <provider> --key <api_key>'."
        );
    }

    let session_id = if continue_last {
        let sessions = harness.session_manager.list(1)?;
        match sessions.first() {
            Some(s) => s.id.clone(),
            None => {
                harness
                    .session_manager
                    .create(agent, "", &workspace_root)?
                    .id
            }
        }
    } else {
        harness
            .session_manager
            .create(agent, "", &workspace_root)?
            .id
    };

    let runtime = AgentRuntime::new(
        agent.to_string(),
        session_id.clone(),
        agent_def.max_turns.unwrap_or(30),
    );
    let mut runtime = runtime.with_logger(&harness);
    runtime.interactive = false;

    let start = std::time::Instant::now();
    let result = runtime.run_turn(&harness, prompt).await?;
    let elapsed = start.elapsed();

    if !result.response.is_empty() {
        println!("{}", result.response);
    }

    let pending = harness
        .background_tasks
        .wait_all(std::time::Duration::from_secs(300))
        .await;
    if pending > 0 {
        eprintln!("⚠ {} background task(s) still running after timeout", pending);
    }

    let tool_time_ms: u64 = result.tool_calls.iter().map(|tc| tc.duration_ms).sum();
    eprintln!(
        "\n─── {} tool call(s) ({:.1}s) │ {}in + {}out tokens │ {:.1}s ───",
        result.tool_calls_made,
        tool_time_ms as f64 / 1000.0,
        result.input_tokens,
        result.output_tokens,
        elapsed.as_secs_f64()
    );

    Ok(())
}

pub(crate) fn register_providers_from_env(harness: &mut runtime::Harness) -> Result<()> {
    if let Ok(key) = env::var("OPENAI_API_KEY") {
        let base_url =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".to_string());
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4.1".to_string());
        let provider = provider::openai_compat::OpenAICompatProvider::new(
            reqwest::Client::new(),
            base_url,
            key,
            model,
        );
        harness
            .provider_registry
            .register("openai", Box::new(provider));
    }

    if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
        let model =
            env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-0".to_string());
        let provider =
            provider::anthropic::AnthropicProvider::new(reqwest::Client::new(), key, model);
        harness
            .provider_registry
            .register("anthropic", Box::new(provider));
    }

    if let Some(oauth_token) = auth::read_copilot_hosts_token() {
        if harness.provider_registry.get("copilot").is_none() {
            let model = env::var("COPILOT_MODEL").ok();
            let provider = provider::copilot::CopilotProvider::new(oauth_token, model);
            harness
                .provider_registry
                .register("copilot", Box::new(provider));
        }
    }

    let creds = auth::Credentials::load().unwrap_or_default();
    for (name, cred) in &creds.providers {
        if harness.provider_registry.get(name).is_some() {
            continue;
        }

        match cred.provider_type {
            auth::ProviderType::OpenAI | auth::ProviderType::Custom => {
                let base_url = cred
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com".to_string());
                let model = cred.model.clone().unwrap_or_else(|| "gpt-4.1".to_string());
                let provider = provider::openai_compat::OpenAICompatProvider::new(
                    reqwest::Client::new(),
                    base_url,
                    cred.api_key.clone(),
                    model,
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            auth::ProviderType::Anthropic => {
                let model = cred
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
                let provider = provider::anthropic::AnthropicProvider::new(
                    reqwest::Client::new(),
                    cred.api_key.clone(),
                    model,
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            auth::ProviderType::Copilot => {
                let provider = provider::copilot::CopilotProvider::new(
                    cred.api_key.clone(),
                    cred.model.clone(),
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
        }
    }

    Ok(())
}
