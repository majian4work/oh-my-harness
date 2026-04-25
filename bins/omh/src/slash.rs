use std::path::Path;

use anyhow::Result;

use crate::tui::input_ast::SlashInvocation;

pub enum SlashResult {
    Response(String),
    Notify(String),
    ListModels { force_refresh: bool },
    ListAgents,
    ListNotifications,
}

pub fn dispatch(invocation: &SlashInvocation, workspace_root: &Path) -> Result<SlashResult> {
    let cmd = invocation.command.as_str();
    let args = invocation.args.as_str();

    match cmd {
        "models" => {
            let force_refresh = args.eq_ignore_ascii_case("refresh");
            Ok(SlashResult::ListModels { force_refresh })
        }
        "agents" => Ok(SlashResult::ListAgents),
        "notifications" => Ok(SlashResult::ListNotifications),
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
            Ok(SlashResult::Notify(format!("Unknown command: /{cmd}")))
        }
    }
}

fn dispatch_evolution(args: &str) -> Result<SlashResult> {
    match args {
        "" => Ok(SlashResult::Notify(
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
        "pause" => Ok(SlashResult::Notify("Evolution paused.".to_string())),
        "resume" => Ok(SlashResult::Notify("Evolution resumed.".to_string())),
        other => Ok(SlashResult::Notify(format!(
            "Unknown evolution command: {other}\nAvailable: log, consolidate, pause, resume"
        ))),
    }
}

fn list_skills(workspace_root: &Path) -> Result<SlashResult> {
    let registry =
        skill::SkillRegistry::load(workspace_root).unwrap_or_else(|_| skill::SkillRegistry::new());

    let all: Vec<_> = registry.all().collect();
    if all.is_empty() {
        return Ok(SlashResult::Notify(
            "No skills found. Add skills to .omh/skills/ or $XDG_CONFIG_HOME/omh/skills/"
                .to_string(),
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
        return Ok(SlashResult::Notify("Usage: /skill <name>".to_string()));
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
                Ok(SlashResult::Notify(format!(
                    "Skill '{name}' not found. No skills configured."
                )))
            } else {
                Ok(SlashResult::Notify(format!(
                    "Skill '{name}' not found. Available: {}",
                    available.join(", ")
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SlashResult, dispatch};
    use crate::tui::input_ast::SlashInvocation;

    fn slash(raw: &str, command: &str, args: &str) -> SlashInvocation {
        SlashInvocation {
            raw: raw.to_string(),
            command: command.to_string(),
            args: args.to_string(),
        }
    }

    #[test]
    fn dispatch_unknown_command_returns_response() {
        let dir = std::env::temp_dir();
        let result = dispatch(&slash("/nope", "nope", ""), &dir).unwrap();
        assert!(matches!(result, SlashResult::Notify(msg) if msg.contains("Unknown command")));
    }

    #[test]
    fn dispatch_models_returns_list_models() {
        let dir = std::env::temp_dir();
        let result = dispatch(&slash("/models", "models", ""), &dir).unwrap();
        assert!(matches!(
            result,
            SlashResult::ListModels {
                force_refresh: false
            }
        ));
    }

    #[test]
    fn dispatch_models_refresh_returns_force_refresh() {
        let dir = std::env::temp_dir();
        let result = dispatch(&slash("/models refresh", "models", "refresh"), &dir).unwrap();
        assert!(matches!(
            result,
            SlashResult::ListModels {
                force_refresh: true
            }
        ));
    }

    #[test]
    fn dispatch_agents_returns_list_agents() {
        let dir = std::env::temp_dir();
        let result = dispatch(&slash("/agents", "agents", ""), &dir).unwrap();
        assert!(matches!(result, SlashResult::ListAgents));
    }
}
