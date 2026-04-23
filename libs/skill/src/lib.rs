mod skill_tool;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

pub use skill_tool::SkillTool;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    Always,
    Auto,
    Semantic,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub activation: Activation,
    pub globs: Vec<String>,
    pub content: String,
    pub source_path: Option<PathBuf>,
    pub bundled_files: Vec<PathBuf>,
}

impl SkillDefinition {
    pub fn matches_files(&self, files: &[&str]) -> bool {
        if self.globs.is_empty() {
            return false;
        }

        for pattern in &self.globs {
            let glob_pattern = glob::Pattern::new(pattern).ok();
            if let Some(pat) = glob_pattern {
                for file in files {
                    if pat.matches(file) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDefinition>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: BTreeMap::new(),
        }
    }

    pub fn load(workspace_root: &Path) -> Result<Self> {
        let mut registry = Self::new();
        registry.register_builtins();

        let global_dir = dirs::config_dir().join("skills");
        registry.discover_dir(&global_dir)?;

        let project_dir = workspace_root.join(".omh/skills");
        registry.discover_dir(&project_dir)?;

        Ok(registry)
    }

    fn register_builtins(&mut self) {
        let builtins: &[(&str, &str)] = &[(
            "update-best-models",
            include_str!("../skills/update-best-models.md"),
        )];
        for (name, content) in builtins {
            if let Ok(skill) = parse_skill_content(name, content) {
                self.register(skill);
            }
        }
    }

    fn discover_dir(&mut self, dir: &Path) -> Result<()> {
        if !dir.exists() || !dir.is_dir() {
            return Ok(());
        }

        let mut discovered = BTreeSet::new();
        for suffix in ["*.md", "*/SKILL.md"] {
            let pattern = dir.join(suffix).to_string_lossy().into_owned();
            let entries = glob::glob(&pattern)
                .with_context(|| format!("failed to glob skill pattern: {pattern}"))?;

            for entry in entries {
                let path = entry
                    .with_context(|| format!("failed to read skill entry in {}", dir.display()))?;
                discovered.insert(path);
            }
        }

        for path in discovered {
            let skill = parse_skill_file(&path)
                .with_context(|| format!("failed to parse skill file {}", path.display()))?;
            self.register(skill);
        }

        Ok(())
    }

    pub fn register(&mut self, skill: SkillDefinition) {
        if !is_valid_slash_name(&skill.name) {
            tracing::warn!(
                "Skipping skill '{}': name must be a single word or hyphenated (no spaces). \
                 Rename the directory/file to use hyphens instead of spaces.",
                skill.name
            );
            return;
        }
        if let Some(existing) = self.skills.get(&skill.name) {
            if existing.source_path.is_none() && skill.source_path.is_some() {
                tracing::warn!(
                    "Ignoring user-defined skill '{}' ({}): conflicts with a builtin skill. \
                     Please rename it.",
                    skill.name,
                    skill.source_path.as_ref().unwrap().display(),
                );
                return;
            }
        }
        self.skills.insert(skill.name.clone(), skill);
    }

    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name)
    }

    pub fn all(&self) -> impl Iterator<Item = &SkillDefinition> {
        self.skills.values()
    }

    pub fn always_active(&self) -> Vec<&SkillDefinition> {
        self.skills
            .values()
            .filter(|skill| skill.activation == Activation::Always)
            .collect()
    }

    pub fn auto_matched(&self, active_files: &[&str]) -> Vec<&SkillDefinition> {
        self.skills
            .values()
            .filter(|skill| {
                skill.activation == Activation::Auto && skill.matches_files(active_files)
            })
            .collect()
    }

    pub fn on_demand(&self) -> Vec<&SkillDefinition> {
        self.skills
            .values()
            .filter(|skill| matches!(skill.activation, Activation::Semantic | Activation::Manual))
            .collect()
    }

    pub fn format_available_skills(&self) -> String {
        let on_demand = self.on_demand();
        if on_demand.is_empty() {
            return String::new();
        }

        let mut output = String::from("Available skills (use the `skill` tool to load):\n");
        for skill in on_demand {
            output.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        output
    }

    pub fn inject_for_context(&self, active_files: &[&str]) -> String {
        let mut parts = Vec::new();

        for skill in self.always_active() {
            parts.push(format!("[Skill: {}]\n{}", skill.name, skill.content));
        }

        for skill in self.auto_matched(active_files) {
            parts.push(format!("[Skill: {}]\n{}", skill.name, skill.content));
        }

        if parts.is_empty() {
            return String::new();
        }

        parts.join("\n\n")
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_skill_file(path: &Path) -> Result<SkillDefinition> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read skill file {}", path.display()))?;
    let mut skill = parse_skill_content(&default_skill_name(path), &raw)?;
    skill.source_path = Some(path.to_path_buf());
    skill.bundled_files = bundled_files_for(path)?;
    Ok(skill)
}

fn parse_skill_content(default_name: &str, raw: &str) -> Result<SkillDefinition> {
    let normalized = raw.replace("\r\n", "\n");
    let (frontmatter, content) = split_frontmatter(&normalized);
    let metadata = parse_frontmatter(frontmatter.as_deref().unwrap_or(""))?;

    Ok(SkillDefinition {
        name: metadata.name.unwrap_or_else(|| default_name.to_string()),
        description: metadata.description.unwrap_or_default(),
        activation: metadata.activation.unwrap_or(Activation::Semantic),
        globs: metadata.globs.unwrap_or_default(),
        content: content.trim().to_string(),
        source_path: None,
        bundled_files: vec![],
    })
}

#[derive(Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    activation: Option<Activation>,
    globs: Option<Vec<String>>,
}

fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    if let Some(rest) = raw.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return (
                Some(rest[..end].to_string()),
                rest[(end + "\n---\n".len())..].to_string(),
            );
        }

        if let Some(frontmatter_only) = rest.strip_suffix("\n---") {
            return (Some(frontmatter_only.to_string()), String::new());
        }
    }

    (None, raw.to_string())
}

fn parse_frontmatter(frontmatter: &str) -> Result<Frontmatter> {
    let mut parsed = Frontmatter::default();
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }

        if let Some(value) = line.strip_prefix("globs:") {
            let value = value.trim();
            if value.is_empty() {
                let mut globs = Vec::new();
                index += 1;

                while index < lines.len() {
                    let nested = lines[index].trim();
                    if nested.is_empty() {
                        index += 1;
                        continue;
                    }

                    if let Some(item) = nested.strip_prefix("- ") {
                        globs.push(parse_scalar(item));
                        index += 1;
                        continue;
                    }

                    break;
                }

                parsed.globs = Some(globs);
                continue;
            }

            parsed.globs = Some(parse_inline_list(value)?);
            index += 1;
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => parsed.name = Some(parse_scalar(value)),
                "description" => parsed.description = Some(parse_scalar(value)),
                "activation" => parsed.activation = Some(parse_activation(value)?),
                _ => {}
            }
        }

        index += 1;
    }

    Ok(parsed)
}

fn parse_inline_list(value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| anyhow!("invalid list syntax: {value}"))?;

    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }

    Ok(inner.split(',').map(parse_scalar).collect())
}

fn parse_scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            return value[1..value.len() - 1].to_string();
        }
    }

    value.to_string()
}

fn parse_activation(value: &str) -> Result<Activation> {
    match parse_scalar(value).to_ascii_lowercase().as_str() {
        "always" => Ok(Activation::Always),
        "auto" => Ok(Activation::Auto),
        "semantic" => Ok(Activation::Semantic),
        "manual" => Ok(Activation::Manual),
        other => Err(anyhow!("unsupported activation: {other}")),
    }
}

fn default_skill_name(path: &Path) -> String {
    if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("skill")
            .to_string();
    }

    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string()
}

fn bundled_files_for(path: &Path) -> Result<Vec<PathBuf>> {
    if path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md") {
        return Ok(Vec::new());
    }

    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };

    let mut bundled_files = Vec::new();
    for entry in fs::read_dir(parent).with_context(|| {
        format!(
            "failed to scan bundled skill directory {}",
            parent.display()
        )
    })? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path != path {
            bundled_files.push(entry_path);
        }
    }
    bundled_files.sort();

    Ok(bundled_files)
}

fn is_valid_slash_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(' ')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ::tool::{ToolContext, ToolHandler};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn parses_skill_file_with_frontmatter() {
        let dir = unique_temp_dir("parse-skill");
        let path = dir.join("rust-testing.md");
        fs::write(
            &path,
            "---\nname: rust-testing\ndescription: \"Testing conventions for Rust projects\"\nglobs: [\"*_test.rs\", \"tests/**/*.rs\"]\nactivation: auto\n---\n\nSkill content goes here.\n",
        )
        .unwrap();

        let skill = parse_skill_file(&path).unwrap();

        assert_eq!(skill.name, "rust-testing");
        assert_eq!(skill.description, "Testing conventions for Rust projects");
        assert_eq!(skill.activation, Activation::Auto);
        assert_eq!(skill.globs, vec!["*_test.rs", "tests/**/*.rs"]);
        assert_eq!(skill.content, "Skill content goes here.");
        assert_eq!(skill.source_path.as_deref(), Some(path.as_path()));
        assert!(skill.bundled_files.is_empty());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovers_skills_from_directory_layouts() {
        let dir = unique_temp_dir("discover-skills");
        let skills_dir = dir.join(".omh/skills");
        fs::create_dir_all(skills_dir.join("packaged")).unwrap();
        fs::write(
            skills_dir.join("flat.md"),
            "---\ndescription: Flat skill\nactivation: semantic\n---\nFlat content\n",
        )
        .unwrap();
        fs::write(
            skills_dir.join("packaged/SKILL.md"),
            "---\ndescription: Packaged skill\nactivation: manual\n---\nPackaged content\n",
        )
        .unwrap();
        fs::write(skills_dir.join("packaged/notes.txt"), "extra").unwrap();

        let mut registry = SkillRegistry::new();
        registry.discover_dir(&skills_dir).unwrap();

        let names = registry
            .all()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["flat", "packaged"]);
        assert_eq!(
            registry
                .get("packaged")
                .unwrap()
                .bundled_files
                .iter()
                .map(|path| path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["notes.txt"]
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn filters_skills_by_activation_and_globs() {
        let mut registry = SkillRegistry::new();
        registry.register(skill("always", Activation::Always, vec![], "always"));
        registry.register(skill(
            "auto",
            Activation::Auto,
            vec!["tests/**/*.rs"],
            "auto",
        ));
        registry.register(skill("semantic", Activation::Semantic, vec![], "semantic"));
        registry.register(skill("manual", Activation::Manual, vec![], "manual"));

        assert_eq!(
            registry
                .always_active()
                .into_iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["always"]
        );
        assert_eq!(
            registry
                .auto_matched(&["tests/unit/example.rs", "src/lib.rs"])
                .into_iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["auto"]
        );
        assert_eq!(
            registry
                .on_demand()
                .into_iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["manual", "semantic"]
        );
    }

    #[test]
    fn formats_available_skills_listing() {
        let mut registry = SkillRegistry::new();
        registry.register(skill("semantic", Activation::Semantic, vec![], "semantic"));
        registry.register(skill("manual", Activation::Manual, vec![], "manual"));

        assert_eq!(
            registry.format_available_skills(),
            "Available skills (use the `skill` tool to load):\n- manual: manual description\n- semantic: semantic description\n"
        );
    }

    #[test]
    fn injects_always_and_auto_skill_content() {
        let mut registry = SkillRegistry::new();
        registry.register(skill(
            "always",
            Activation::Always,
            vec![],
            "Always content",
        ));
        registry.register(skill(
            "auto",
            Activation::Auto,
            vec!["src/**/*.rs"],
            "Auto content",
        ));
        registry.register(skill(
            "manual",
            Activation::Manual,
            vec![],
            "Manual content",
        ));

        assert_eq!(
            registry.inject_for_context(&["src/main.rs"]),
            "[Skill: always]\nAlways content\n\n[Skill: auto]\nAuto content"
        );
    }

    #[tokio::test]
    async fn skill_tool_loads_existing_skill_and_errors_for_missing() {
        let mut registry = SkillRegistry::new();
        registry.register(skill(
            "semantic",
            Activation::Semantic,
            vec![],
            "semantic body",
        ));
        registry.register(SkillDefinition {
            name: "manual".to_string(),
            description: "manual description".to_string(),
            activation: Activation::Manual,
            globs: Vec::new(),
            content: "manual body".to_string(),
            source_path: None,
            bundled_files: vec![PathBuf::from("bundle.txt")],
        });

        let tool = SkillTool::new(Arc::new(registry));
        let ctx = tool_context();

        let loaded = tool
            .execute(serde_json::json!({ "name": "manual" }), &ctx)
            .await
            .unwrap();
        assert!(!loaded.is_error);
        assert!(loaded.content.contains("<skill_content name=\"manual\">"));
        assert!(loaded.content.contains("manual body"));
        assert!(loaded.content.contains("- bundle.txt"));

        let missing = tool
            .execute(serde_json::json!({ "name": "missing" }), &ctx)
            .await
            .unwrap();
        assert!(missing.is_error);
        assert!(missing.content.contains("Skill 'missing' not found"));
        assert!(missing.content.contains("manual"));
        assert!(missing.content.contains("semantic"));
    }

    fn skill(
        name: &str,
        activation: Activation,
        globs: Vec<&str>,
        content: &str,
    ) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            description: format!("{name} description"),
            activation,
            globs: globs.into_iter().map(str::to_string).collect(),
            content: content.to_string(),
            source_path: None,
            bundled_files: Vec::new(),
        }
    }

    fn tool_context() -> ToolContext {
        ToolContext {
            session_id: "session-1".to_string(),
            message_id: "message-1".to_string(),
            agent_name: "agent".to_string(),
            workspace_root: PathBuf::from("/tmp"),
            session_dir: None,
            abort: CancellationToken::new(),
            depth: 0,
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("skill-{label}-{}", ulid::Ulid::new()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rejects_skill_names_with_spaces() {
        let mut registry = SkillRegistry::new();
        registry.register(skill("valid-name", Activation::Always, vec![], "content"));
        registry.register(skill("has space", Activation::Always, vec![], "content"));
        registry.register(skill("also_valid", Activation::Always, vec![], "content"));

        let names: Vec<&str> = registry.all().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"valid-name"));
        assert!(names.contains(&"also_valid"));
        assert!(!names.contains(&"has space"));
    }
}
