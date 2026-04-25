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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct AgentFrontMatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_user_invocable")]
    user_invocable: bool,
    #[serde(default)]
    can_delegate_to: Vec<String>,
    #[serde(default)]
    config: Vec<(String, String)>,
    #[serde(default)]
    use_when: Vec<String>,
    #[serde(default)]
    avoid_when: Vec<String>,
    #[serde(default)]
    triggers: Vec<(String, String)>,
    #[serde(default)]
    permissions: Vec<(String, String)>,
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
    #[serde(default = "default_user_invocable")]
    pub user_invocable: bool,
    #[serde(default)]
    pub can_delegate_to: Vec<String>,
    pub system_prompt: String,
    pub model: Option<ModelSpec>,
    pub permission_rules: PermissionPolicy,
    pub max_turns: Option<u32>,
    pub temperature: Option<f32>,
    pub metadata: AgentMetadata,
    pub source: AgentSource,
}

impl AgentDefinition {
    pub fn is_primary_switchable(&self) -> bool {
        self.mode == AgentMode::Primary
    }

    pub fn is_explicitly_invocable(&self) -> bool {
        self.user_invocable
    }

    pub fn allows_delegation_to(&self, agent_name: &str) -> bool {
        self.can_delegate_to.iter().any(|name| name == agent_name)
    }
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

        paths.push(dirs::config_dir().join("agents"));

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
            .filter(|agent| !agent.is_primary_switchable())
            .collect()
    }

    pub fn primary_switchable_agents(&self) -> Vec<&AgentDefinition> {
        self.agents
            .values()
            .filter(|agent| agent.is_primary_switchable())
            .collect()
    }

    pub fn explicit_invocation_candidates(&self) -> Vec<&AgentDefinition> {
        self.agents
            .values()
            .filter(|agent| agent.is_explicitly_invocable())
            .collect()
    }

    pub fn delegation_allowed(&self, agent_name: &str, target_name: &str) -> bool {
        self.get(agent_name)
            .is_some_and(|agent| agent.allows_delegation_to(target_name))
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
    let (front_matter, body) = parse_agent_front_matter(&content)?;

    let name = front_matter
        .name
        .context("missing 'name' in front matter")?;
    let description = front_matter
        .description
        .unwrap_or_else(|| format!("Agent '{name}'"));

    let system_prompt = body.trim().to_string();
    if system_prompt.is_empty() {
        bail!("agent body (system prompt) cannot be empty");
    }

    let config: Vec<(String, String)> = front_matter
        .config
        .into_iter()
        .map(|(k, v)| (normalize_key(&k), v))
        .collect();
    let permissions: Vec<(String, String)> = front_matter
        .permissions
        .into_iter()
        .map(|(k, v)| (normalize_key(&k), v))
        .collect();
    let use_when = front_matter.use_when;
    let avoid_when = front_matter.avoid_when;
    let triggers = front_matter.triggers;

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
        user_invocable: front_matter.user_invocable,
        can_delegate_to: front_matter.can_delegate_to,
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
            model: model("claude-haiku-4.5"),
            temperature: Some(0.2),
        },
        Category {
            name: "deep".to_string(),
            description: "Higher-depth reasoning for complex implementation work.".to_string(),
            model: model("claude-opus-4.6"),
            temperature: Some(0.2),
        },
        Category {
            name: "visual-engineering".to_string(),
            description: "UI, interaction, and visual product engineering tasks.".to_string(),
            model: model("claude-sonnet-4.6"),
            temperature: Some(0.3),
        },
        Category {
            name: "ultrabrain".to_string(),
            description: "Maximum-effort analysis for the hardest problems.".to_string(),
            model: model("claude-opus-4.6"),
            temperature: Some(0.1),
        },
        Category {
            name: "artistry".to_string(),
            description: "Creative ideation and style-sensitive generation.".to_string(),
            model: model("claude-sonnet-4.5"),
            temperature: Some(0.8),
        },
        Category {
            name: "writing".to_string(),
            description: "Editorial writing, summaries, and communication tasks.".to_string(),
            model: model("claude-sonnet-4.6"),
            temperature: Some(0.6),
        },
        Category {
            name: "unspecified-low".to_string(),
            description: "Fallback low-cost category when no better match exists.".to_string(),
            model: model("claude-haiku-4.5"),
            temperature: Some(0.3),
        },
        Category {
            name: "unspecified-high".to_string(),
            description: "Fallback high-capability category when no better match exists."
                .to_string(),
            model: model("claude-sonnet-4.6"),
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

fn parse_agent_front_matter(content: &str) -> Result<(AgentFrontMatter, &str)> {
    let Some(content) = content.strip_prefix("---\n") else {
        return Ok((AgentFrontMatter::default_with_user_invocable(), content));
    };

    let (block, rest) = content
        .split_once("\n---\n")
        .context("missing closing '---' for agent front matter")?;
    let mut front_matter = AgentFrontMatter::default_with_user_invocable();
    let mut lines = block.lines().peekable();

    while let Some(raw_line) = lines.next() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let (key, value) = line
            .split_once(':')
            .with_context(|| format!("expected '<key>: <value>' entry, got: {line}"))?;
        match normalize_key(key).as_str() {
            "name" => {
                front_matter.name = Some(value.trim().to_string());
            }
            "description" => {
                front_matter.description = Some(value.trim().to_string());
            }
            "userinvocable" => {
                front_matter.user_invocable = parse_bool(value.trim())?;
            }
            "candelegateto" => {
                front_matter.can_delegate_to = parse_string_list(value.trim(), &mut lines)?;
            }
            "config" => {
                front_matter.config = parse_nested_key_values(value.trim(), &mut lines)?;
            }
            "usewhen" => {
                front_matter.use_when = parse_string_list(value.trim(), &mut lines)?;
            }
            "avoidwhen" => {
                front_matter.avoid_when = parse_string_list(value.trim(), &mut lines)?;
            }
            "triggers" => {
                front_matter.triggers = parse_nested_key_values(value.trim(), &mut lines)?;
            }
            "permissions" => {
                front_matter.permissions = parse_nested_key_values(value.trim(), &mut lines)?;
            }
            other => bail!("unsupported front matter key: {other}"),
        }
    }

    Ok((front_matter, rest))
}

impl AgentFrontMatter {
    fn default_with_user_invocable() -> Self {
        Self {
            name: None,
            description: None,
            user_invocable: default_user_invocable(),
            can_delegate_to: Vec::new(),
            config: Vec::new(),
            use_when: Vec::new(),
            avoid_when: Vec::new(),
            triggers: Vec::new(),
            permissions: Vec::new(),
        }
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match normalize_key(value).as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("invalid boolean value: {value}"),
    }
}

fn parse_string_list<'a, I>(value: &str, lines: &mut std::iter::Peekable<I>) -> Result<Vec<String>>
where
    I: Iterator<Item = &'a str>,
{
    if value.is_empty() {
        let mut items = Vec::new();
        while let Some(next_line) = lines.peek().copied() {
            let trimmed = next_line.trim();
            if trimmed.is_empty() {
                lines.next();
                continue;
            }

            if let Some(item) = trimmed.strip_prefix("- ") {
                items.push(item.trim().to_string());
                lines.next();
                continue;
            }

            break;
        }
        return Ok(items);
    }

    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return Ok(split_csv(inner));
    }

    bail!("invalid list value: {value}")
}

fn parse_nested_key_values<'a, I>(
    value: &str,
    lines: &mut std::iter::Peekable<I>,
) -> Result<Vec<(String, String)>>
where
    I: Iterator<Item = &'a str>,
{
    if !value.is_empty() {
        bail!("expected nested block, got inline value: {value}");
    }
    let mut items = Vec::new();
    while let Some(next_line) = lines.peek().copied() {
        let trimmed = next_line.trim();
        if trimmed.is_empty() {
            lines.next();
            continue;
        }
        if !next_line.starts_with(' ') && !next_line.starts_with('\t') {
            break;
        }
        if let Some((key, val)) = trimmed.split_once(':') {
            items.push((key.trim().to_string(), val.trim().to_string()));
            lines.next();
        } else {
            break;
        }
    }
    Ok(items)
}

fn default_user_invocable() -> bool {
    true
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
            r#"---
name: sample-agent
description: A sample agent used in tests.
config:
  mode: subagent
  cost: cheap
  model: gpt-4o-mini
  max_turns: 30
  temperature: 0.3
permissions:
  allow: read_file, grep, glob
  deny: bash(rm:*)
use_when:
  - description1
  - description2
avoid_when:
  - description
---
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
    fn parses_metadata_from_frontmatter() {
        let agent = parse_agent_markdown(
            r#"---
name: meta-agent
description: Read-only advisor
user_invocable: true
can_delegate_to: []
config:
  mode: subagent
  cost: expensive
  model: claude-sonnet-4.6
  max_turns: 50
  temperature: 0.1
use_when:
  - Need architecture guidance.
  - Need debugging analysis.
avoid_when:
  - Task requires editing files.
triggers:
  architecture: Advise on system design
  debug: Analyze root causes
---
You are the meta agent.
"#,
            AgentSource::Builtin,
        )
        .unwrap();

        assert_eq!(agent.name, "meta-agent");
        assert_eq!(agent.mode, AgentMode::Subagent);
        assert_eq!(agent.cost, AgentCost::Expensive);
        assert_eq!(agent.max_turns, Some(50));
        assert_eq!(agent.temperature, Some(0.1));
        assert_eq!(agent.model.unwrap().model_id, "claude-sonnet-4.6");
        assert_eq!(
            agent.metadata.use_when,
            vec!["Need architecture guidance.", "Need debugging analysis."]
        );
        assert_eq!(
            agent.metadata.avoid_when,
            vec!["Task requires editing files."]
        );
        assert_eq!(
            agent.metadata.triggers,
            vec![
                (
                    "architecture".to_string(),
                    "Advise on system design".to_string()
                ),
                ("debug".to_string(), "Analyze root causes".to_string()),
            ]
        );
    }

    #[test]
    fn parses_front_matter_metadata_and_defaults_user_invocable() {
        let agent = parse_agent_markdown(
            r#"---
name: sample-agent
user_invocable: false
can_delegate_to:
  - worker
  - oracle
config:
  mode: subagent
  cost: cheap
  model: gpt-4o-mini
---
You are the test agent.
"#,
            AgentSource::UserDefined(PathBuf::from("sample.md")),
        )
        .unwrap();

        assert!(!agent.user_invocable);
        assert_eq!(agent.can_delegate_to, vec!["worker", "oracle"]);
    }

    #[test]
    fn defaults_custom_agents_to_user_invocable_when_missing() {
        let agent = parse_agent_markdown(
            r#"---
name: sample-agent
config:
  mode: subagent
  cost: cheap
  model: gpt-4o-mini
---
You are the test agent.
"#,
            AgentSource::UserDefined(PathBuf::from("sample.md")),
        )
        .unwrap();

        assert!(agent.user_invocable);
        assert!(agent.can_delegate_to.is_empty());
    }

    #[test]
    fn rejects_malformed_front_matter_metadata() {
        let error = parse_agent_markdown(
            r#"---
unknown: true
---
You are the test agent.
"#,
            AgentSource::UserDefined(PathBuf::from("sample.md")),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported front matter key: unknown")
        );
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
        assert!(registry.get("orchestrator").unwrap().user_invocable);
        assert_eq!(
            registry.get("orchestrator").unwrap().can_delegate_to,
            vec![
                "worker",
                "oracle",
                "explore",
                "librarian",
                "planner",
                "reviewer"
            ]
        );
    }

    #[test]
    fn registry_exposes_filtered_agent_sets_from_metadata() {
        let registry = AgentRegistry::load_from_paths(Vec::<PathBuf>::new()).unwrap();

        let primary_names = registry
            .primary_switchable_agents()
            .into_iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(primary_names, vec!["orchestrator", "planner"]);

        let invocable_names = registry
            .explicit_invocation_candidates()
            .into_iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            invocable_names,
            vec![
                "explore",
                "librarian",
                "oracle",
                "orchestrator",
                "planner",
                "reviewer",
                "worker",
            ]
        );

        assert!(!registry.delegation_allowed("worker", "oracle"));
        assert!(registry.delegation_allowed("orchestrator", "oracle"));
        assert!(registry.delegation_allowed("orchestrator", "reviewer"));
    }

    #[test]
    fn agent_definition_exposes_delegation_allowlist_checks() {
        let agent = parse_agent_markdown(
            r#"---
name: sample-agent
user_invocable: true
can_delegate_to: [worker, oracle]
config:
  mode: primary
  cost: cheap
---
You are the test agent.
"#,
            AgentSource::UserDefined(PathBuf::from("sample.md")),
        )
        .unwrap();

        assert!(agent.is_primary_switchable());
        assert!(agent.is_explicitly_invocable());
        assert!(agent.allows_delegation_to("worker"));
        assert!(!agent.allows_delegation_to("reviewer"));
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
