use std::path::Path;

use anyhow::Result;

use crate::tui::input_ast::SlashInvocation;

pub enum SlashResult {
    Response(String),
    Notify(String),
    ListModels,
    ListAgents,
    ListNotifications,
    SwitchEffort(provider::Effort),
    /// Run a skill: inject skill content as context and execute a turn with the prompt.
    RunSkill {
        skill_name: String,
        prompt: String,
    },
}

pub fn dispatch(invocation: &SlashInvocation, workspace_root: &Path) -> Result<SlashResult> {
    let cmd = invocation.command.as_str();
    let args = invocation.args.as_str();

    match cmd {
        "models" => Ok(SlashResult::ListModels),
        "agents" => Ok(SlashResult::ListAgents),
        "effort" => dispatch_effort(args),
        "notifications" => Ok(SlashResult::ListNotifications),
        "evolution" | "evolve" => dispatch_evolution(args),
        _ => {
            let registry = skill::SkillRegistry::load(workspace_root)
                .unwrap_or_else(|_| skill::SkillRegistry::new());
            if registry.get(cmd).is_some() {
                let prompt = args.trim().to_string();
                if prompt.is_empty() {
                    return Ok(SlashResult::Notify(format!("Usage: /{cmd} <prompt>")));
                }
                return Ok(SlashResult::RunSkill {
                    skill_name: cmd.to_string(),
                    prompt,
                });
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

fn dispatch_effort(args: &str) -> Result<SlashResult> {
    let level = args.trim().to_lowercase();
    match level.as_str() {
        "low" => Ok(SlashResult::SwitchEffort(provider::Effort::Low)),
        "default" => Ok(SlashResult::SwitchEffort(provider::Effort::Default)),
        "high" => Ok(SlashResult::SwitchEffort(provider::Effort::High)),
        "" => Ok(SlashResult::Notify(
            "Usage: /effort <low|default|high>".to_string(),
        )),
        other => Ok(SlashResult::Notify(format!(
            "Unknown effort level: {other}\nAvailable: low, default, high"
        ))),
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
        assert!(matches!(result, SlashResult::ListModels));
    }

    #[test]
    fn dispatch_agents_returns_list_agents() {
        let dir = std::env::temp_dir();
        let result = dispatch(&slash("/agents", "agents", ""), &dir).unwrap();
        assert!(matches!(result, SlashResult::ListAgents));
    }
}
