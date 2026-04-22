use std::path::Path;

use anyhow::Result;

use crate::auth::{ActiveModel, Credentials, OmhConfig, check_env_providers, mask_key};

pub enum SlashResult {
    Response(String),
    AuthPopup,
    ListModels { force_refresh: bool },
    NotACommand,
}

pub fn dispatch(input: &str, workspace_root: &Path) -> Result<SlashResult> {
    let input = input.trim();
    if !input.starts_with('/') {
        return Ok(SlashResult::NotACommand);
    }

    let mut parts = input[1..].splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    match cmd {
        "help" => Ok(SlashResult::Response(help_text(workspace_root))),
        "auth" => dispatch_auth(args),
        "model" => set_model(args),
        "models" => {
            let force_refresh = args.eq_ignore_ascii_case("refresh");
            Ok(SlashResult::ListModels { force_refresh })
        }
        "evolution" | "evolve" => dispatch_evolution(args),
        "skills" => list_skills(workspace_root),
        "skill" => show_skill(args, workspace_root),
        _ => {
            let registry = skill::SkillRegistry::load(workspace_root)
                .unwrap_or_else(|_| skill::SkillRegistry::new());
            if let Some(skill) = registry.get(cmd) {
                return Ok(SlashResult::Response(format!(
                    "── skill: {} ──\n{}\n\n{}",
                    skill.name, skill.description, skill.content
                )));
            }
            Ok(SlashResult::Response(format!(
                "Unknown command: /{cmd}\nType /help for available commands."
            )))
        }
    }
}

fn help_text(workspace_root: &Path) -> String {
    let mut lines = vec![
        "Available commands:".to_string(),
        "  /help                  Show this help".to_string(),
        "  /auth login            Add provider credential (interactive)".to_string(),
        "  /auth logout <name>    Remove provider credential".to_string(),
        "  /auth list             List configured providers".to_string(),
        "  /auth status           Show provider status".to_string(),
        "  /models                List available models (cached, 24h TTL)".to_string(),
        "  /models refresh        Force refresh model list from providers".to_string(),
        "  /model <p>/<id>        Set active model (e.g. /model openai/gpt-4.1)".to_string(),
        "  /evolution log         Show evolution history".to_string(),
        "  /evolution consolidate Consolidate learnings".to_string(),
        "  /skills                List available skills".to_string(),
        "  /skill <name>          Show skill content".to_string(),
    ];

    let registry =
        skill::SkillRegistry::load(workspace_root).unwrap_or_else(|_| skill::SkillRegistry::new());
    let on_demand: Vec<_> = registry.on_demand().into_iter().collect();
    if !on_demand.is_empty() {
        lines.push(String::new());
        lines.push("Skill shortcuts:".to_string());
        for s in on_demand {
            lines.push(format!("  /{:<20} {}", s.name, s.description));
        }
    }

    lines.join("\n")
}

fn set_model(args: &str) -> Result<SlashResult> {
    if args.is_empty() {
        return Ok(SlashResult::Response(
            "Usage: /model <provider>/<model_id>\nUse /models to list available models."
                .to_string(),
        ));
    }

    let Some((provider_id, model_id)) = args.split_once('/') else {
        return Ok(SlashResult::Response(
            "Format: /model <provider>/<model_id>".to_string(),
        ));
    };

    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return Ok(SlashResult::Response(
            "Format: /model <provider>/<model_id>".to_string(),
        ));
    }

    let mut config = OmhConfig::load()?;
    config.active_model = Some(ActiveModel {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    });
    config.save()?;

    Ok(SlashResult::Response(format!(
        "✓ Active model set to {provider_id}/{model_id}"
    )))
}

fn dispatch_auth(args: &str) -> Result<SlashResult> {
    let mut parts = args.splitn(2, ' ');
    let sub = parts.next().unwrap_or("");
    let sub_args = parts.next().unwrap_or("").trim();

    match sub {
        "" => Ok(SlashResult::AuthPopup),
        "login" => {
            if sub_args.eq_ignore_ascii_case("copilot") {
                return Ok(SlashResult::Response(
                    "GitHub Copilot requires browser authentication. Run `omh auth login copilot` from your terminal."
                        .to_string(),
                ));
            }
            Ok(SlashResult::AuthPopup)
        }
        "logout" => {
            if sub_args.is_empty() {
                return Ok(SlashResult::Response(
                    "Usage: /auth logout <provider>".to_string(),
                ));
            }
            let mut creds = Credentials::load()?;
            if creds.remove(sub_args) {
                creds.save()?;
                Ok(SlashResult::Response(format!(
                    "✓ Provider '{sub_args}' removed"
                )))
            } else {
                Ok(SlashResult::Response(format!(
                    "Provider '{sub_args}' not found in credentials"
                )))
            }
        }
        "list" => {
            let creds = Credentials::load()?;
            if creds.providers.is_empty() {
                return Ok(SlashResult::Response(
                    "No providers in credentials. Use /auth login to add one.".to_string(),
                ));
            }
            let mut lines = vec!["Configured providers:".to_string()];
            let mut names: Vec<_> = creds.providers.keys().cloned().collect();
            names.sort();
            for name in names {
                let cred = creds.get(&name).unwrap();
                let model_info = cred.model.as_deref().unwrap_or("default");
                lines.push(format!(
                    "  {} ({:?}) key={} model={}",
                    name,
                    cred.provider_type,
                    mask_key(&cred.api_key),
                    model_info,
                ));
            }
            Ok(SlashResult::Response(lines.join("\n")))
        }
        "status" => {
            let creds = Credentials::load()?;
            let env_keys = check_env_providers();
            let mut lines = vec![
                "Provider Status:".to_string(),
                "─────────────────────────────────────".to_string(),
            ];

            for (name, source) in &env_keys {
                lines.push(format!("  ✓ {} (from {})", name, source));
            }
            for name in creds.providers.keys() {
                if !env_keys.iter().any(|(n, _)| n == name) {
                    lines.push(format!("  ✓ {} (from credentials.json)", name));
                }
            }

            if env_keys.is_empty() && creds.providers.is_empty() {
                lines.push("  No providers configured.".to_string());
                lines.push("  Use /auth login to add one.".to_string());
            }
            Ok(SlashResult::Response(lines.join("\n")))
        }
        _ => Ok(SlashResult::Response(format!(
            "Unknown auth command: {sub}\nAvailable: login, logout, list, status"
        ))),
    }
}

fn dispatch_evolution(args: &str) -> Result<SlashResult> {
    match args {
        "" => Ok(SlashResult::Response(
            "Usage: /evolution <log|consolidate|pause|resume>".to_string(),
        )),
        "log" => Ok(SlashResult::Response(
            "Evolution log: not yet connected to runtime.\nUse `omh evolution log` from CLI."
                .to_string(),
        )),
        "consolidate" => Ok(SlashResult::Response(
            "Evolution consolidate: not yet connected to runtime.\nUse `omh evolution consolidate` from CLI."
                .to_string(),
        )),
        "pause" => Ok(SlashResult::Response("Evolution paused.".to_string())),
        "resume" => Ok(SlashResult::Response("Evolution resumed.".to_string())),
        other => Ok(SlashResult::Response(format!(
            "Unknown evolution command: {other}\nAvailable: log, consolidate, pause, resume"
        ))),
    }
}

fn list_skills(workspace_root: &Path) -> Result<SlashResult> {
    let registry =
        skill::SkillRegistry::load(workspace_root).unwrap_or_else(|_| skill::SkillRegistry::new());

    let all: Vec<_> = registry.all().collect();
    if all.is_empty() {
        return Ok(SlashResult::Response(
            "No skills found.\nAdd skills to .omh/skills/ or ~/.config/omh/skills/".to_string(),
        ));
    }

    let mut lines = vec!["Available skills:".to_string()];
    for s in all {
        let mode = match s.activation {
            skill::Activation::Always => "always",
            skill::Activation::Auto => "auto",
            skill::Activation::Semantic => "semantic",
            skill::Activation::Manual => "manual",
        };
        lines.push(format!("  {:<20} [{}] {}", s.name, mode, s.description));
    }
    lines.push(String::new());
    lines.push("Use /skill <name> to view content.".to_string());
    Ok(SlashResult::Response(lines.join("\n")))
}

fn show_skill(name: &str, workspace_root: &Path) -> Result<SlashResult> {
    if name.is_empty() {
        return Ok(SlashResult::Response("Usage: /skill <name>".to_string()));
    }

    let registry =
        skill::SkillRegistry::load(workspace_root).unwrap_or_else(|_| skill::SkillRegistry::new());

    match registry.get(name) {
        Some(s) => Ok(SlashResult::Response(format!(
            "── {} ({:?}) ──\n{}\n\n{}",
            s.name, s.activation, s.description, s.content
        ))),
        None => {
            let available: Vec<_> = registry.all().map(|s| s.name.clone()).collect();
            if available.is_empty() {
                Ok(SlashResult::Response(format!(
                    "Skill '{name}' not found. No skills configured."
                )))
            } else {
                Ok(SlashResult::Response(format!(
                    "Skill '{name}' not found.\nAvailable: {}",
                    available.join(", ")
                )))
            }
        }
    }
}
