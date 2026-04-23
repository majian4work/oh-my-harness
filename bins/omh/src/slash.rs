use std::path::Path;

use anyhow::Result;

pub enum SlashResult {
    Response(String),
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
            Ok(SlashResult::Response(format!("Unknown command: /{cmd}")))
        }
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
            "No skills found.\nAdd skills to .omh/skills/ or $XDG_CONFIG_HOME/omh/skills/"
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
