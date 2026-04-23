use anyhow::{Context, Result, bail};
use dialoguer::{Input, Password, Select};
use memory::{MemoryEntry, MemoryKind, MemorySource, Scope};
use provider::{CompletionRequest, SystemMessage};
use runtime::harness::AgentOverride;

use crate::{
    AuthCmd, EvolutionCmd, MemoryCmd, SnapshotCmd,
    auth::{
        self, Credentials, ModelsCache, ProviderCredential, check_env_providers, mask_key,
        provider_type_for_name,
    },
};

pub async fn cmd_sessions(limit: usize) -> Result<()> {
    let harness = crate::init_harness()?;
    let summaries = harness.session_manager.list(limit)?;

    if summaries.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    println!(
        "{:<40} {:<14} {:<8} {:<6} {}",
        "ID", "Agent", "Model", "Msgs", "Title"
    );
    println!("{}", "─".repeat(90));
    for s in &summaries {
        let title = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        };
        let model_short = if s.model.len() > 12 {
            &s.model[..12]
        } else {
            &s.model
        };
        println!(
            "{:<40} {:<14} {:<8} {:<6} {}",
            s.id, s.agent_name, model_short, s.message_count, title
        );
    }
    println!("\n{} session(s)", summaries.len());

    Ok(())
}

pub async fn cmd_memory(cmd: MemoryCmd) -> Result<()> {
    let harness = crate::init_harness()?;

    match cmd {
        MemoryCmd::List => {
            let entries = harness.memory.list(&Scope::Global, None).await?;
            if entries.is_empty() {
                println!("No memory entries.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] ({:?}/{:?}) {}",
                        entry.id,
                        entry.scope,
                        entry.kind,
                        truncate(&entry.content, 80)
                    );
                }
                println!("\n{} entries", entries.len());
            }
        }
        MemoryCmd::Search { query } => {
            let entries = harness.memory.recall(&query, &Scope::Global, 10).await?;
            if entries.is_empty() {
                println!("No matches for '{query}'.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] ({:?}) {}",
                        entry.id,
                        entry.scope,
                        truncate(&entry.content, 80)
                    );
                }
            }
        }
        MemoryCmd::Add { content } => {
            let now = now_millis();
            let entry = MemoryEntry {
                id: ulid::Ulid::new(),
                scope: Scope::Global,
                kind: MemoryKind::Fact,
                content,
                source: MemorySource::UserAuthored,
                confidence: 1.0,
                reinforcement_count: 0,
                supersedes: None,
                created_at: now,
                updated_at: now,
            };
            harness.memory.remember(entry).await?;
            println!("Memory entry added.");
        }
        MemoryCmd::Forget { id } => {
            let ulid: ulid::Ulid = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid ULID: {id}"))?;
            harness.memory.forget(ulid).await?;
            println!("Memory entry forgotten.");
        }
    }

    Ok(())
}

pub async fn cmd_evolution(cmd: EvolutionCmd) -> Result<()> {
    let harness = crate::init_harness()?;

    match cmd {
        EvolutionCmd::Log => {
            let entries = harness
                .memory
                .list(&Scope::Global, Some(MemoryKind::Rule))
                .await?;
            if entries.is_empty() {
                println!("No evolution entries.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] confidence={:.2} {}",
                        entry.id,
                        entry.confidence,
                        truncate(&entry.content, 70)
                    );
                }
            }
        }
        EvolutionCmd::Revert { id } => {
            let ulid: ulid::Ulid = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid ULID: {id}"))?;
            harness.memory.forget(ulid).await?;
            println!("Evolution entry reverted (forgotten).");
        }
        EvolutionCmd::Consolidate => {
            let report = harness.evolution.consolidate(&Scope::Global)?;
            println!(
                "Consolidation: merged={}, pruned={}",
                report.merged, report.pruned
            );
        }
        EvolutionCmd::Pause => {
            println!("Evolution paused (not persisted — restart re-enables).");
        }
        EvolutionCmd::Resume => {
            println!("Evolution resumed.");
        }
    }

    Ok(())
}

pub async fn cmd_snapshot(cmd: SnapshotCmd) -> Result<()> {
    let _harness = crate::init_harness()?;

    match cmd {
        SnapshotCmd::List { session_id } => {
            println!("Snapshots for session {session_id}:");
            println!("  (snapshot listing requires git log integration — use `git log` for now)");
        }
        SnapshotCmd::Diff { snapshot_id } => {
            println!("Diff for snapshot {snapshot_id}:");
            println!("  (use `git diff {snapshot_id}` directly)");
        }
        SnapshotCmd::Revert { snapshot_id } => {
            println!("Reverting to snapshot {snapshot_id}...");
            let workspace_root = std::env::current_dir()?;
            let mgr = snapshot::GitSnapshot::new(&workspace_root);
            let snap_id = snapshot::SnapshotId(snapshot_id.clone());
            mgr.revert_to(&snap_id)?;
            println!("Reverted to {snapshot_id}.");
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.replace('\n', " ")
    } else {
        format!("{}…", s[..max].replace('\n', " "))
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn cmd_auth(cmd: AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Login {
            provider,
            key,
            base_url,
            model,
        } => {
            let provider = match provider {
                Some(p) => p,
                None => select_provider()?,
            };
            let key = if provider == "copilot" {
                match key {
                    Some(k) => k,
                    None => copilot_login().await?,
                }
            } else {
                match key {
                    Some(k) => k,
                    None => prompt_api_key(&provider)?,
                }
            };
            let provider_type = provider_type_for_name(&provider);
            let mut creds = Credentials::load()?;
            creds.add(
                provider.clone(),
                ProviderCredential {
                    provider_type,
                    api_key: key,
                    base_url,
                    model,
                },
            );
            creds.save()?;
            println!("✓ Provider '{}' configured successfully", provider);
            println!(
                "  Credentials saved to {}",
                Credentials::global_path().display()
            );
        }
        AuthCmd::Logout { provider } => {
            let mut creds = Credentials::load()?;
            if creds.remove(&provider) {
                creds.save()?;
                println!("✓ Provider '{}' removed", provider);
            } else {
                println!("Provider '{}' not found in credentials", provider);
            }
        }
        AuthCmd::List => {
            let creds = Credentials::load()?;
            if creds.providers.is_empty() {
                println!("No providers configured.");
                println!("Use 'omh auth login <provider> --key <api_key>' to add one.");
            } else {
                println!("Configured providers:");
                let mut names: Vec<_> = creds.providers.keys().cloned().collect();
                names.sort();
                for name in names {
                    let cred = creds
                        .get(&name)
                        .expect("provider name collected from credentials must exist");
                    let model_info = cred.model.as_deref().unwrap_or("default");
                    println!(
                        "  {} ({:?}) key={} model={}",
                        name,
                        cred.provider_type,
                        mask_key(&cred.api_key),
                        model_info
                    );
                }
            }
        }
        AuthCmd::Status => {
            let creds = Credentials::load()?;
            let env_keys = check_env_providers();

            println!("Provider Status:");
            println!("─────────────────────────────────────");

            for (name, source) in &env_keys {
                println!("  ✓ {} (from {})", name, source);
            }
            for name in creds.providers.keys() {
                if !env_keys.iter().any(|(env_name, _)| env_name == name) {
                    println!("  ✓ {} (from credentials.json)", name);
                }
            }

            if env_keys.is_empty() && creds.providers.is_empty() {
                println!("  No providers configured.");
                println!("\n  Set environment variables (OPENAI_API_KEY, ANTHROPIC_API_KEY)");
                println!("  Or use: omh auth login <provider> --key <api_key>");
            }
        }
    }

    Ok(())
}

async fn copilot_login() -> Result<String> {
    println!("Starting GitHub OAuth device flow...");
    let client = reqwest::Client::new();
    let device = auth::start_device_flow(&client).await?;

    println!("\n  Open: {}", device.verification_uri);
    println!("  Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");

    let token = auth::poll_for_access_token(&client, &device.device_code, device.interval).await?;
    println!("✓ Authorization successful!");
    Ok(token)
}

const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI (GPT-4, GPT-4.1, o3, ...)"),
    ("anthropic", "Anthropic (Claude Sonnet, Opus, ...)"),
    ("copilot", "GitHub Copilot"),
    ("custom", "Custom OpenAI-compatible endpoint"),
];

fn select_provider() -> Result<String> {
    let items: Vec<&str> = KNOWN_PROVIDERS.iter().map(|(_, desc)| *desc).collect();
    let selection = Select::new()
        .with_prompt("Select a provider")
        .items(&items)
        .default(0)
        .interact()?;

    let (name, _) = KNOWN_PROVIDERS[selection];
    if name == "custom" {
        let custom_name: String = Input::new().with_prompt("Provider name").interact_text()?;
        if custom_name.is_empty() {
            bail!("provider name cannot be empty");
        }
        Ok(custom_name)
    } else {
        Ok(name.to_string())
    }
}

fn prompt_api_key(provider: &str) -> Result<String> {
    let key = Password::new()
        .with_prompt(format!("API key for {provider}"))
        .interact()?;
    if key.is_empty() {
        bail!("API key cannot be empty");
    }
    Ok(key)
}

pub async fn cmd_update_best_models(global: bool) -> Result<()> {
    let mut harness = crate::init_harness()?;
    crate::cli::register_providers_from_env(&mut harness)?;

    if harness.provider_registry.list().is_empty() {
        bail!("no provider configured. Run 'omh auth login' first.");
    }

    println!("Refreshing model cache from all providers (validating accessibility)...");
    let old_cache = ModelsCache::load();
    let fresh_models = harness.provider_registry.list_all_models_validated().await;

    if fresh_models.is_empty() {
        bail!("no models returned from any provider");
    }

    let mut new_cache = ModelsCache::load();
    new_cache.update(&fresh_models);
    new_cache.save()?;

    let changed = has_model_changes(&old_cache, &new_cache);
    if !changed {
        println!("No model changes detected. Nothing to do.");
        return Ok(());
    }

    println!("Model changes detected. Asking LLM for optimal agent assignments...");

    let builtin_agents: Vec<_> = harness
        .agent_registry
        .all()
        .into_iter()
        .filter(|a| matches!(a.source, agent::AgentSource::Builtin))
        .collect();

    let mut agent_info = String::new();
    for a in &builtin_agents {
        let current_model = a
            .model
            .as_ref()
            .map(|m| m.model_id.as_str())
            .unwrap_or("none");
        agent_info.push_str(&format!(
            "- {} ({:?}): {}, cost={:?}, current_model={}\n",
            a.name, a.mode, a.description, a.cost, current_model
        ));
    }

    let mut model_info = String::new();
    for (provider_id, models) in &fresh_models {
        for m in models {
            let name = m.name.as_deref().unwrap_or(&m.id);
            model_info.push_str(&format!(
                "- provider={}, id={}, name={}\n",
                provider_id, m.id, name
            ));
        }
    }

    let prompt = format!(
        "You are configuring an AI agent orchestration framework.\n\
         Each builtin agent needs the best available model assigned based on its role and cost tier.\n\n\
         ## Builtin Agents\n{agent_info}\n\
         ## Available Models\n{model_info}\n\
         Respond with ONLY a JSON object mapping agent names to their optimal model config.\n\
         Format: {{\"agent_name\": {{\"model\": \"model_id\", \"provider\": \"provider_id\"}}, ...}}\n\
         Rules:\n\
         - Expensive agents (oracle, orchestrator, planner, reviewer) should use the most capable model\n\
         - Cheap/free agents (worker, librarian, explore) should use fast, cost-effective models\n\
         - Only use models from the available list above\n\
         - Include ALL builtin agents in the response"
    );

    let resolved = harness
        .provider_registry
        .resolve_model(None, Some(provider::ModelCostTier::Medium))
        .ok_or_else(|| anyhow::anyhow!("no provider available for LLM call"))?;

    let provider = harness
        .provider_registry
        .get(&resolved.provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider not found: {}", resolved.provider_id))?;

    let request = CompletionRequest {
        model: resolved.model_id.clone(),
        system: vec![SystemMessage {
            content: "You are a helpful assistant. Respond only with valid JSON.".to_string(),
            cache_control: false,
        }],
        messages: vec![message::Message::user("req", prompt)],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(2000),
    };

    let response = provider.complete(request).await?;
    let response_text = response.message.text();

    let json_str = extract_json(&response_text)
        .ok_or_else(|| anyhow::anyhow!("LLM returned invalid response:\n{response_text}"))?;

    let recommendations: std::collections::HashMap<String, AgentModelRecommendation> =
        serde_json::from_str(json_str)
            .with_context(|| format!("failed to parse LLM response as JSON:\n{response_text}"))?;

    let mut overrides = std::collections::HashMap::new();
    let builtin_names: std::collections::BTreeSet<_> =
        builtin_agents.iter().map(|a| a.name.as_str()).collect();

    for (name, rec) in &recommendations {
        if !builtin_names.contains(name.as_str()) {
            eprintln!("  ⚠ skipping unknown agent: {name}");
            continue;
        }
        println!("  {} → {} ({})", name, rec.model, rec.provider);
        overrides.insert(
            name.clone(),
            AgentOverride {
                model: Some(rec.model.clone()),
                provider: Some(rec.provider.clone()),
            },
        );
    }

    if overrides.is_empty() {
        println!("No valid recommendations. Nothing written.");
        return Ok(());
    }

    let workspace_root = std::env::current_dir()?;
    runtime::Harness::write_agent_overrides(&workspace_root, &overrides, global)?;

    let target = if global {
        dirs::config_dir().join("config.toml").display().to_string()
    } else {
        ".omh/config.toml".to_string()
    };
    println!("\nWrote agent overrides to {target}");
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct AgentModelRecommendation {
    model: String,
    provider: String,
}

fn has_model_changes(old: &ModelsCache, new: &ModelsCache) -> bool {
    if old.providers.len() != new.providers.len() {
        return true;
    }
    for (pid, new_entry) in &new.providers {
        match old.providers.get(pid) {
            None => return true,
            Some(old_entry) => {
                let old_ids: std::collections::BTreeSet<_> =
                    old_entry.models.iter().map(|m| &m.id).collect();
                let new_ids: std::collections::BTreeSet<_> =
                    new_entry.models.iter().map(|m| &m.id).collect();
                if old_ids != new_ids {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_json(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.starts_with('{') {
        return Some(text);
    }
    if let Some(start) = text.find("```json") {
        let start = start + 7;
        if let Some(end) = text[start..].find("```") {
            return Some(text[start..start + end].trim());
        }
    }
    if let Some(start) = text.find("```") {
        let start = start + 3;
        let inner = text[start..].trim_start();
        if inner.starts_with('{') {
            if let Some(end) = inner.find("```") {
                return Some(inner[..end].trim());
            }
        }
    }
    if let Some(start) = text.find('{') {
        let rest = &text[start..];
        let mut depth = 0;
        for (i, ch) in rest.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&rest[..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}
