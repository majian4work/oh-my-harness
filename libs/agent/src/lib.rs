use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use glob::glob;
use permission::{PermissionPolicy, PermissionRule};
use serde::{Deserialize, Serialize};
use tool::PermissionLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    Primary,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCost {
    Free,
    Cheap,
    Expensive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentMetadata {
    pub use_when: Vec<String>,
    pub avoid_when: Vec<String>,
    pub triggers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSource {
    Builtin,
    UserDefined(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model_id: String,
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub mode: AgentMode,
    pub cost: AgentCost,
    pub system_prompt: String,
    pub model: Option<ModelSpec>,
    pub permission_rules: PermissionPolicy,
    pub max_turns: Option<u32>,
    pub temperature: Option<f32>,
    pub metadata: AgentMetadata,
    pub source: AgentSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    pub description: String,
    pub model: ModelSpec,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
}

impl AgentRegistry {
    pub fn load(workspace_root: impl AsRef<Path>) -> Result<Self> {
        let workspace_root = workspace_root.as_ref();
        let mut paths = Vec::new();

        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(home).join(".config/omh/agents"));
        }

        paths.push(workspace_root.join(".omh/agents"));

        Self::load_from_paths(paths)
    }

    pub fn load_from_paths<I, P>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut registry = Self::from_builtin();

        for path in paths {
            registry.load_path(path.as_ref())?;
        }

        registry.refresh_orchestrator_prompt();
        Ok(registry)
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    pub fn all(&self) -> Vec<&AgentDefinition> {
        self.agents.values().collect()
    }

    pub fn subagents(&self) -> Vec<&AgentDefinition> {
        self.agents
            .values()
            .filter(|agent| agent.mode == AgentMode::Subagent)
            .collect()
    }

    pub fn all_metadata(&self) -> Vec<(&str, &AgentMetadata)> {
        self.agents
            .values()
            .map(|agent| (agent.name.as_str(), &agent.metadata))
            .collect()
    }

    fn from_builtin() -> Self {
        let mut agents = BTreeMap::new();

        for agent in builtin_agent_definitions() {
            agents.insert(agent.name.clone(), agent);
        }

        let mut registry = Self { agents };
        registry.refresh_orchestrator_prompt();
        registry
    }

    fn load_path(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        if path.is_file() {
            self.load_file(path)?;
            return Ok(());
        }

        let pattern = path.join("*.md");
        let pattern = pattern.to_string_lossy().into_owned();
        let mut files = Vec::new();

        for entry in glob(&pattern).with_context(|| format!("invalid glob pattern: {pattern}"))? {
            let file = entry
                .with_context(|| format!("failed to read agent path from pattern: {pattern}"))?;
            files.push(file);
        }

        files.sort();

        for file in files {
            self.load_file(&file)?;
        }

        Ok(())
    }

    fn load_file(&mut self, path: &Path) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read agent definition: {}", path.display()))?;
        let agent = parse_agent_definition(&content, AgentSource::UserDefined(path.to_path_buf()))?;

        if let Some(existing) = self.agents.get(&agent.name) {
            if matches!(existing.source, AgentSource::Builtin) {
                tracing::warn!(
                    "Ignoring user-defined agent '{}' ({}): conflicts with a builtin agent. \
                     Please rename it.",
                    agent.name,
                    path.display(),
                );
                return Ok(());
            }
        }

        self.agents.insert(agent.name.clone(), agent);
        Ok(())
    }

    fn refresh_orchestrator_prompt(&mut self) {
        let should_refresh = matches!(
            self.agents.get("orchestrator"),
            Some(agent) if matches!(agent.source, AgentSource::Builtin)
        );

        if !should_refresh {
            return;
        }

        let prompt = generate_orchestrator_prompt(self);

        if let Some(orchestrator) = self.agents.get_mut("orchestrator") {
            orchestrator.system_prompt = prompt;
        }
    }
}

pub fn parse_agent_file(path: impl AsRef<Path>) -> Result<AgentDefinition> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read agent definition: {}", path.display()))?;
    parse_agent_definition(&content, AgentSource::UserDefined(path.to_path_buf()))
}

pub fn parse_agent_markdown(content: &str, source: AgentSource) -> Result<AgentDefinition> {
    parse_agent_definition(content, source)
}

pub fn parse_agent_definition(content: &str, source: AgentSource) -> Result<AgentDefinition> {
    let content = content.replace("\r\n", "\n");
    let mut name = None;
    let mut description_lines = Vec::new();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current_section: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();

        if name.is_none() {
            if line.trim().is_empty() {
                continue;
            }

            if let Some(value) = line.trim().strip_prefix("# ") {
                let parsed_name = value.trim();
                if parsed_name.is_empty() {
                    bail!("agent name heading cannot be empty");
                }
                name = Some(parsed_name.to_string());
                continue;
            }

            bail!("agent definition must start with '# <name>'");
        }

        if current_section.as_deref() != Some("system prompt") {
            if let Some(section_name) = line.trim().strip_prefix("## ") {
                current_section = Some(section_name.trim().to_ascii_lowercase());
                sections
                    .entry(current_section.clone().unwrap())
                    .or_default();
                continue;
            }
        }

        match current_section.as_ref() {
            Some(section_name) => {
                let section = sections.entry(section_name.clone()).or_default();
                if !section.is_empty() {
                    section.push('\n');
                }
                section.push_str(line);
            }
            None => description_lines.push(line.to_string()),
        }
    }

    let name = name.context("missing agent name heading")?;
    let config = parse_key_value_section(
        sections
            .get("config")
            .map(String::as_str)
            .unwrap_or_default(),
    )?;
    let permissions = parse_key_value_section(
        sections
            .get("permissions")
            .map(String::as_str)
            .unwrap_or_default(),
    )?;
    let use_when = parse_bullet_section(
        sections
            .get("use when")
            .map(String::as_str)
            .unwrap_or_default(),
    );
    let avoid_when = parse_bullet_section(
        sections
            .get("avoid when")
            .map(String::as_str)
            .unwrap_or_default(),
    );
    let triggers = parse_trigger_section(
        sections
            .get("triggers")
            .map(String::as_str)
            .unwrap_or_default(),
    )?;
    let system_prompt = sections
        .get("system prompt")
        .map(|prompt| prompt.trim().to_string())
        .filter(|prompt| !prompt.is_empty())
        .context("missing '## System Prompt' section")?;

    let description = normalize_text_block(&description_lines.join("\n"));
    let description = if !description.is_empty() {
        description
    } else if let Some(first_use) = use_when.first() {
        first_use.clone()
    } else {
        format!("User-defined agent '{name}'")
    };

    let mut mode = AgentMode::Subagent;
    let mut cost = AgentCost::Cheap;
    let mut model_id = None;
    let mut provider_id = None;
    let mut max_turns = None;
    let mut temperature = None;
    let mut permission_level = PermissionLevel::ReadOnly;

    for (key, value) in config {
        match key.as_str() {
            "mode" => mode = parse_agent_mode(&value)?,
            "cost" => cost = parse_agent_cost(&value)?,
            "model" => model_id = Some(value),
            "provider" => provider_id = Some(value),
            "maxturns" => {
                max_turns = Some(
                    value
                        .parse::<u32>()
                        .with_context(|| format!("invalid max_turns value: {value}"))?,
                )
            }
            "temperature" => {
                temperature = Some(
                    value
                        .parse::<f32>()
                        .with_context(|| format!("invalid temperature value: {value}"))?,
                )
            }
            "permissionlevel" => permission_level = parse_permission_level(&value)?,
            other => bail!("unsupported config key: {other}"),
        }
    }

    let mut permission_rules = PermissionPolicy {
        default_level: permission_level,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        allow_rules: Vec::new(),
    };

    for (key, value) in permissions {
        let rules = split_csv(&value);
        match key.as_str() {
            "allow" => {
                permission_rules
                    .allow_rules
                    .extend(rules.into_iter().map(PermissionRule::allow));
            }
            "deny" => {
                permission_rules
                    .deny_rules
                    .extend(rules.into_iter().map(PermissionRule::deny));
            }
            "ask" => {
                permission_rules
                    .ask_rules
                    .extend(rules.into_iter().map(PermissionRule::ask));
            }
            other => bail!("unsupported permission key: {other}"),
        }
    }

    Ok(AgentDefinition {
        name,
        description,
        mode,
        cost,
        system_prompt,
        model: model_id.map(|model_id| ModelSpec {
            model_id,
            provider_id,
        }),
        permission_rules,
        max_turns,
        temperature,
        metadata: AgentMetadata {
            use_when,
            avoid_when,
            triggers,
        },
        source,
    })
}

pub fn builtin_agent_definitions() -> Vec<AgentDefinition> {
    const BUILTIN_AGENTS: &[&str] = &[
        include_str!("../agents/orchestrator.md"),
        include_str!("../agents/worker.md"),
        include_str!("../agents/oracle.md"),
        include_str!("../agents/explore.md"),
        include_str!("../agents/librarian.md"),
        include_str!("../agents/planner.md"),
        include_str!("../agents/reviewer.md"),
    ];

    BUILTIN_AGENTS
        .iter()
        .map(|content| {
            parse_agent_definition(content, AgentSource::Builtin)
                .expect("builtin agent markdown is invalid")
        })
        .collect()
}

pub fn default_categories() -> Vec<Category> {
    vec![
        Category {
            name: "quick".to_string(),
            description: "Fast, low-cost execution for straightforward tasks.".to_string(),
            model: model("gpt-5.4-mini"),
            temperature: Some(0.2),
        },
        Category {
            name: "deep".to_string(),
            description: "Higher-depth reasoning for complex implementation work.".to_string(),
            model: model("gpt-5.4"),
            temperature: Some(0.2),
        },
        Category {
            name: "visual-engineering".to_string(),
            description: "UI, interaction, and visual product engineering tasks.".to_string(),
            model: model("gpt-4.1"),
            temperature: Some(0.3),
        },
        Category {
            name: "ultrabrain".to_string(),
            description: "Maximum-effort analysis for the hardest problems.".to_string(),
            model: model("o3"),
            temperature: Some(0.1),
        },
        Category {
            name: "artistry".to_string(),
            description: "Creative ideation and style-sensitive generation.".to_string(),
            model: model("gpt-4.1"),
            temperature: Some(0.8),
        },
        Category {
            name: "writing".to_string(),
            description: "Editorial writing, summaries, and communication tasks.".to_string(),
            model: model("gpt-5.4"),
            temperature: Some(0.6),
        },
        Category {
            name: "unspecified-low".to_string(),
            description: "Fallback low-cost category when no better match exists.".to_string(),
            model: model("gpt-5.4-mini"),
            temperature: Some(0.3),
        },
        Category {
            name: "unspecified-high".to_string(),
            description: "Fallback high-capability category when no better match exists."
                .to_string(),
            model: model("gpt-5.4"),
            temperature: Some(0.3),
        },
    ]
}

pub fn generate_orchestrator_prompt(registry: &AgentRegistry) -> String {
    let mut lines = vec![
        "You are the orchestrator agent.".to_string(),
        "Delegate focused work to subagents when they are a better fit than doing the work yourself.".to_string(),
        "Use the table below to choose the right subagent.".to_string(),
        String::new(),
        "| Name | Cost | Description | Use When | Avoid When | Triggers |".to_string(),
        "| --- | --- | --- | --- | --- | --- |".to_string(),
    ];

    for agent in registry.subagents() {
        let use_when = join_or_dash(&agent.metadata.use_when);
        let avoid_when = join_or_dash(&agent.metadata.avoid_when);
        let triggers = if agent.metadata.triggers.is_empty() {
            "-".to_string()
        } else {
            agent
                .metadata
                .triggers
                .iter()
                .map(|(trigger, meaning)| format!("{trigger} → {meaning}"))
                .collect::<Vec<_>>()
                .join("; ")
        };

        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            agent.name,
            format_cost(agent.cost),
            sanitize_table_cell(&agent.description),
            sanitize_table_cell(&use_when),
            sanitize_table_cell(&avoid_when),
            sanitize_table_cell(&triggers),
        ));
    }

    lines.join("\n")
}

fn parse_agent_mode(value: &str) -> Result<AgentMode> {
    match normalize_key(value).as_str() {
        "primary" => Ok(AgentMode::Primary),
        "subagent" => Ok(AgentMode::Subagent),
        _ => bail!("invalid agent mode: {value}"),
    }
}

fn parse_agent_cost(value: &str) -> Result<AgentCost> {
    match normalize_key(value).as_str() {
        "free" => Ok(AgentCost::Free),
        "cheap" => Ok(AgentCost::Cheap),
        "expensive" => Ok(AgentCost::Expensive),
        _ => bail!("invalid agent cost: {value}"),
    }
}

fn parse_permission_level(value: &str) -> Result<PermissionLevel> {
    match normalize_key(value).as_str() {
        "readonly" => Ok(PermissionLevel::ReadOnly),
        "workspacewrite" => Ok(PermissionLevel::WorkspaceWrite),
        "fullaccess" => Ok(PermissionLevel::FullAccess),
        _ => bail!("invalid permission level: {value}"),
    }
}

fn parse_key_value_section(section: &str) -> Result<Vec<(String, String)>> {
    let mut values = Vec::new();

    for line in section.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let item = line
            .strip_prefix("- ")
            .with_context(|| format!("expected bullet list item, got: {line}"))?;
        let (key, value) = item
            .split_once(':')
            .with_context(|| format!("expected '<key>: <value>' entry, got: {line}"))?;
        values.push((normalize_key(key), value.trim().to_string()));
    }

    Ok(values)
}

fn parse_bullet_section(section: &str) -> Vec<String> {
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_trigger_section(section: &str) -> Result<Vec<(String, String)>> {
    let mut triggers = Vec::new();

    for line in parse_bullet_section(section) {
        let (trigger, meaning) = line
            .split_once(':')
            .with_context(|| format!("invalid trigger entry: {line}"))?;
        triggers.push((trigger.trim().to_string(), meaning.trim().to_string()));
    }

    Ok(triggers)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_text_block(value: &str) -> String {
    value
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join("; ")
    }
}

fn sanitize_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn format_cost(cost: AgentCost) -> &'static str {
    match cost {
        AgentCost::Free => "Free",
        AgentCost::Cheap => "Cheap",
        AgentCost::Expensive => "Expensive",
    }
}

fn model(model_id: impl Into<String>) -> ModelSpec {
    ModelSpec {
        model_id: model_id.into(),
        provider_id: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "agent-crate-tests-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parses_markdown_agent_definition() {
        let temp = TestDir::new("parse");
        let path = temp.path().join("sample.md");
        fs::write(
            &path,
            r#"# sample-agent

A sample agent used in tests.

## Config
- mode: subagent
- cost: cheap
- model: gpt-4o-mini
- max_turns: 30
- temperature: 0.3

## Permissions
- allow: read_file, grep, glob
- deny: bash(rm:*)

## Use When
- description1
- description2

## Avoid When
- description

## System Prompt
You are the test agent.
Stay focused.
"#,
        )
        .unwrap();

        let agent = parse_agent_file(&path).unwrap();

        assert_eq!(agent.name, "sample-agent");
        assert_eq!(agent.description, "A sample agent used in tests.");
        assert_eq!(agent.mode, AgentMode::Subagent);
        assert_eq!(agent.cost, AgentCost::Cheap);
        assert_eq!(agent.max_turns, Some(30));
        assert_eq!(agent.temperature, Some(0.3));
        assert_eq!(agent.model.unwrap().model_id, "gpt-4o-mini");
        assert_eq!(
            agent.permission_rules.default_level,
            PermissionLevel::ReadOnly
        );
        assert_eq!(agent.permission_rules.allow_rules.len(), 3);
        assert_eq!(agent.permission_rules.deny_rules.len(), 1);
        assert_eq!(
            agent.metadata.use_when,
            vec!["description1", "description2"]
        );
        assert_eq!(agent.metadata.avoid_when, vec!["description"]);
        assert_eq!(
            agent.system_prompt,
            "You are the test agent.\nStay focused."
        );
        assert_eq!(agent.source, AgentSource::UserDefined(path));
    }

    #[test]
    fn registry_loads_builtin_agents() {
        let registry = AgentRegistry::load_from_paths(Vec::<PathBuf>::new()).unwrap();

        for name in [
            "orchestrator",
            "worker",
            "oracle",
            "explore",
            "librarian",
            "planner",
            "reviewer",
        ] {
            assert!(
                registry.get(name).is_some(),
                "missing builtin agent: {name}"
            );
        }

        assert_eq!(
            registry.get("orchestrator").unwrap().mode,
            AgentMode::Primary
        );
        assert_eq!(registry.get("explore").unwrap().cost, AgentCost::Free);
    }

    #[test]
    fn orchestrator_prompt_lists_all_subagents() {
        let registry = AgentRegistry::load_from_paths(Vec::<PathBuf>::new()).unwrap();
        let prompt = generate_orchestrator_prompt(&registry);

        for name in ["worker", "oracle", "explore", "librarian", "reviewer"] {
            assert!(prompt.contains(name), "missing subagent in prompt: {name}");
        }

        assert!(
            prompt.contains("| Name | Cost | Description | Use When | Avoid When | Triggers |")
        );
        assert!(
            registry
                .get("orchestrator")
                .unwrap()
                .system_prompt
                .contains("reviewer")
        );
    }

    #[test]
    fn category_defaults_are_populated() {
        let categories = default_categories();
        let names = categories
            .iter()
            .map(|category| category.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "quick",
                "deep",
                "visual-engineering",
                "ultrabrain",
                "artistry",
                "writing",
                "unspecified-low",
                "unspecified-high",
            ]
        );
        assert!(
            categories
                .iter()
                .all(|category| !category.description.is_empty())
        );
        assert!(
            categories
                .iter()
                .all(|category| !category.model.model_id.is_empty())
        );
    }
}
