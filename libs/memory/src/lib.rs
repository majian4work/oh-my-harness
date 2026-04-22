use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use glob::glob;
use regex::Regex;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Ulid,
    pub scope: Scope,
    pub kind: MemoryKind,
    pub content: String,
    pub source: MemorySource,
    pub confidence: f32,
    pub reinforcement_count: u32,
    pub supersedes: Option<Ulid>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Scope {
    Global,
    Project(String),
    Agent(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MemoryKind {
    Rule,
    Preference,
    Decision,
    Pattern,
    Fact,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MemorySource {
    UserAuthored,
    UserCorrection { session_id: String },
    Extracted { session_id: String },
    Evolution,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, entry: MemoryEntry) -> Result<()>;
    async fn recall(&self, query: &str, scope: &Scope, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn forget(&self, id: Ulid) -> Result<()>;
    async fn supersede(&self, old_id: Ulid, new: MemoryEntry) -> Result<()>;
    async fn reinforce(&self, id: Ulid) -> Result<()>;
    async fn list(&self, scope: &Scope, kind: Option<MemoryKind>) -> Result<Vec<MemoryEntry>>;
}

pub struct MarkdownMemoryStore {
    base_dir: PathBuf,
    workspace_root: PathBuf,
}

impl MarkdownMemoryStore {
    pub fn open(workspace_root: &Path) -> Result<Self> {
        let workspace_root = workspace_root.to_path_buf();
        let base_dir = workspace_root.join(".omh/memory");

        fs::create_dir_all(base_dir.join("global"))?;
        fs::create_dir_all(base_dir.join("project"))?;
        fs::create_dir_all(base_dir.join("agent"))?;

        Ok(Self {
            base_dir,
            workspace_root,
        })
    }

    fn scope_dir(&self, scope: &Scope) -> PathBuf {
        match scope {
            Scope::Global => self.base_dir.join("global"),
            Scope::Project(project) => {
                let current_project = project_scope_value(&self.workspace_root);
                let project_key = if project == &current_project {
                    current_project
                } else {
                    project.clone()
                };
                self.base_dir
                    .join("project")
                    .join(format!("{:016x}", stable_hash(project_key.as_bytes())))
            }
            Scope::Agent(agent) => self.base_dir.join("agent").join(agent),
        }
    }

    fn entry_path(&self, scope: &Scope, id: &Ulid) -> PathBuf {
        self.scope_dir(scope).join(format!("{id}.md"))
    }

    fn write_entry(&self, entry: &MemoryEntry) -> Result<()> {
        let path = self.entry_path(&entry.scope, &entry.id);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("memory entry path missing parent: {}", path.display()))?;
        fs::create_dir_all(parent)?;

        let supersedes = entry
            .supersedes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string());
        let serialized = format!(
            "---\nid: {}\nscope: {}\nkind: {}\nsource: {}\nconfidence: {}\nreinforcement_count: {}\nsupersedes: {}\ncreated_at: {}\nupdated_at: {}\n---\n{}",
            entry.id,
            encode_scope(&entry.scope),
            encode_kind(&entry.kind),
            encode_source(&entry.source),
            entry.confidence,
            entry.reinforcement_count,
            supersedes,
            entry.created_at,
            entry.updated_at,
            entry.content,
        );

        fs::write(&path, serialized)
            .with_context(|| format!("failed to write memory entry {}", path.display()))
    }

    fn read_entry(&self, path: &Path) -> Result<MemoryEntry> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read memory entry {}", path.display()))?;

        let mut lines = raw.lines();
        if lines.next() != Some("---") {
            bail!("memory entry {} missing frontmatter start", path.display());
        }

        let mut frontmatter = Vec::new();
        let mut found_end = false;
        for line in &mut lines {
            if line == "---" {
                found_end = true;
                break;
            }
            frontmatter.push(line);
        }

        if !found_end {
            bail!("memory entry {} missing frontmatter end", path.display());
        }

        let body = lines.collect::<Vec<_>>().join("\n");
        let mut id = None;
        let mut scope = None;
        let mut kind = None;
        let mut source = None;
        let mut confidence = None;
        let mut reinforcement_count = None;
        let mut supersedes = None;
        let mut created_at = None;
        let mut updated_at = None;

        for line in frontmatter {
            let (key, value) = line.split_once(':').with_context(|| {
                format!("invalid frontmatter line in {}: {line}", path.display())
            })?;
            let value = value.trim();
            match key.trim() {
                "id" => id = Some(parse_ulid(value)?),
                "scope" => scope = Some(decode_scope(value)?),
                "kind" => kind = Some(decode_kind(value)?),
                "source" => source = Some(decode_source(value)?),
                "confidence" => {
                    confidence = Some(value.parse::<f32>().with_context(|| {
                        format!("invalid confidence in {}: {value}", path.display())
                    })?)
                }
                "reinforcement_count" => {
                    reinforcement_count = Some(value.parse::<u32>().with_context(|| {
                        format!(
                            "invalid reinforcement_count in {}: {value}",
                            path.display()
                        )
                    })?)
                }
                "supersedes" => {
                    supersedes = Some(if value == "null" {
                        None
                    } else {
                        Some(parse_ulid(value)?)
                    })
                }
                "created_at" => {
                    created_at = Some(value.parse::<i64>().with_context(|| {
                        format!("invalid created_at in {}: {value}", path.display())
                    })?)
                }
                "updated_at" => {
                    updated_at = Some(value.parse::<i64>().with_context(|| {
                        format!("invalid updated_at in {}: {value}", path.display())
                    })?)
                }
                other => bail!("unexpected frontmatter key in {}: {other}", path.display()),
            }
        }

        Ok(MemoryEntry {
            id: id.ok_or_else(|| anyhow!("memory entry {} missing id", path.display()))?,
            scope: scope.ok_or_else(|| anyhow!("memory entry {} missing scope", path.display()))?,
            kind: kind.ok_or_else(|| anyhow!("memory entry {} missing kind", path.display()))?,
            content: body,
            source: source
                .ok_or_else(|| anyhow!("memory entry {} missing source", path.display()))?,
            confidence: confidence
                .ok_or_else(|| anyhow!("memory entry {} missing confidence", path.display()))?,
            reinforcement_count: reinforcement_count.ok_or_else(|| {
                anyhow!("memory entry {} missing reinforcement_count", path.display())
            })?,
            supersedes: supersedes
                .ok_or_else(|| anyhow!("memory entry {} missing supersedes", path.display()))?,
            created_at: created_at
                .ok_or_else(|| anyhow!("memory entry {} missing created_at", path.display()))?,
            updated_at: updated_at
                .ok_or_else(|| anyhow!("memory entry {} missing updated_at", path.display()))?,
        })
    }

    fn all_entries(&self, scope: &Scope) -> Result<Vec<MemoryEntry>> {
        let scope_dir = self.scope_dir(scope);
        if !scope_dir.exists() {
            return Ok(Vec::new());
        }

        let pattern = scope_dir.join("*.md").to_string_lossy().into_owned();
        let mut entries = Vec::new();
        for path in glob(&pattern).with_context(|| format!("invalid glob pattern {pattern}"))? {
            entries.push(self.read_entry(&path?)?);
        }
        Ok(entries)
    }

    fn find_entry(&self, id: Ulid) -> Result<(PathBuf, MemoryEntry)> {
        let encoded_id = id.to_string();
        let patterns = [
            self.base_dir.join("global").join(format!("{encoded_id}.md")),
            self.base_dir.join("project/*").join(format!("{encoded_id}.md")),
            self.base_dir.join("agent/*").join(format!("{encoded_id}.md")),
        ];

        for pattern in patterns {
            let pattern = pattern.to_string_lossy().into_owned();
            for path in glob(&pattern).with_context(|| format!("invalid glob pattern {pattern}"))? {
                let path = path?;
                let entry = self.read_entry(&path)?;
                return Ok((path, entry));
            }
        }

        bail!("memory entry {id} not found")
    }
}

#[async_trait]
impl MemoryStore for MarkdownMemoryStore {
    async fn remember(&self, entry: MemoryEntry) -> Result<()> {
        self.write_entry(&entry)
    }

    async fn recall(&self, query: &str, scope: &Scope, limit: usize) -> Result<Vec<MemoryEntry>> {
        let query_tokens: HashSet<&str> = query
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| w.len() >= 2)
            .collect();
        let query_lower = query.trim().to_lowercase();

        let mut entries = self.all_entries(scope)?;
        if query_tokens.is_empty() && query_lower.is_empty() {
            entries.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
            });
            entries.truncate(limit);
            return Ok(entries);
        }

        let mut scored: Vec<(f32, MemoryEntry)> = entries
            .into_iter()
            .filter_map(|entry| {
                let content_lower = entry.content.to_lowercase();
                let content_tokens: HashSet<&str> = content_lower
                    .split_whitespace()
                    .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
                    .filter(|w| w.len() >= 2)
                    .collect();

                let intersection = query_tokens.intersection(&content_tokens).count();
                let union = query_tokens.union(&content_tokens).count();
                let jaccard = if union > 0 { intersection as f32 / union as f32 } else { 0.0 };

                let substring_bonus = if content_lower.contains(&query_lower) { 0.3 } else { 0.0 };

                let score = jaccard + substring_bonus;
                if score > 0.0 {
                    Some((score * entry.confidence, entry))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
        });
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn forget(&self, id: Ulid) -> Result<()> {
        let (path, _) = self.find_entry(id)?;
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete memory entry {}", path.display()))?;
        Ok(())
    }

    async fn supersede(&self, old_id: Ulid, mut new: MemoryEntry) -> Result<()> {
        let (_, mut old) = self.find_entry(old_id)?;
        new.supersedes = Some(old_id);
        let now = unix_timestamp()?;

        old.supersedes = Some(new.id);
        old.updated_at = now;
        self.write_entry(&old)?;
        self.write_entry(&new)?;
        Ok(())
    }

    async fn reinforce(&self, id: Ulid) -> Result<()> {
        let (_, mut entry) = self.find_entry(id)?;
        let now = unix_timestamp()?;

        entry.reinforcement_count += 1;
        entry.confidence = (entry.confidence + 0.05).min(1.0);
        entry.updated_at = now;
        self.write_entry(&entry)
    }

    async fn list(&self, scope: &Scope, kind: Option<MemoryKind>) -> Result<Vec<MemoryEntry>> {
        let mut entries = self.all_entries(scope)?;
        if let Some(kind) = kind {
            entries.retain(|entry| entry.kind == kind);
        }

        entries.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        Ok(entries)
    }
}

pub struct MarkdownMemoryLoader {
    workspace_root: PathBuf,
    home_dir: Option<PathBuf>,
}

impl MarkdownMemoryLoader {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            home_dir: default_home_dir(),
        }
    }

    pub fn with_home_dir(workspace_root: impl Into<PathBuf>, home_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            home_dir: Some(home_dir.into()),
        }
    }

    pub fn project_hash(&self) -> String {
        let project_key = project_scope_value(&self.workspace_root);
        format!("{:016x}", stable_hash(project_key.as_bytes()))
    }

    pub fn load(&self) -> Result<Vec<MemoryEntry>> {
        let mut files = Vec::new();

        let workspace_rules = self.workspace_root.join(".omh/rules.md");
        if workspace_rules.exists() {
            files.push((
                workspace_rules,
                Scope::Project(project_scope_value(&self.workspace_root)),
            ));
        }

        files.extend(self.glob_scoped_files(
            self.workspace_root.join(".omh/rules/*.md"),
            Scope::Project(project_scope_value(&self.workspace_root)),
        )?);

        if let Some(home_dir) = &self.home_dir {
            files.extend(
                self.glob_scoped_files(home_dir.join(".omh/memory/global/*.md"), Scope::Global)?,
            );

            files.extend(self.glob_scoped_files(
                home_dir.join(format!(".omh/memory/projects/{}/*.md", self.project_hash())),
                Scope::Project(project_scope_value(&self.workspace_root)),
            )?);
        }

        files.sort_by(|left, right| left.0.cmp(&right.0));

        let heading_pattern = Regex::new(r"^##\s+(?P<content>.+?)\s*$")?;
        let bullet_pattern = Regex::new(r"^\s*-\s+(?P<content>.+?)\s*$")?;

        let mut entries = Vec::new();
        for (path, scope) in files {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read markdown memory {}", path.display()))?;
            let timestamp = file_timestamp(&path).unwrap_or_else(|_| unix_timestamp().unwrap_or(0));

            for line in content.lines() {
                let captured = heading_pattern
                    .captures(line)
                    .or_else(|| bullet_pattern.captures(line));

                let Some(captured) = captured else {
                    continue;
                };

                let memory_content = captured["content"].trim();
                if memory_content.is_empty() {
                    continue;
                }

                entries.push(MemoryEntry {
                    id: Ulid::new(),
                    scope: scope.clone(),
                    kind: MemoryKind::Rule,
                    content: memory_content.to_string(),
                    source: MemorySource::UserAuthored,
                    confidence: 1.0,
                    reinforcement_count: 0,
                    supersedes: None,
                    created_at: timestamp,
                    updated_at: timestamp,
                });
            }
        }

        Ok(entries)
    }

    fn glob_scoped_files(&self, pattern: PathBuf, scope: Scope) -> Result<Vec<(PathBuf, Scope)>> {
        let pattern = pattern.to_string_lossy().into_owned();
        let mut files = Vec::new();
        for entry in glob(&pattern).with_context(|| format!("invalid glob pattern {pattern}"))? {
            files.push((entry?, scope.clone()));
        }
        Ok(files)
    }
}

pub async fn recall_for_prompt(
    store: &dyn MemoryStore,
    scopes: &[Scope],
    query: &str,
    token_budget: usize,
) -> Result<String> {
    let mut ranked = Vec::new();
    let recall_limit = scopes.len().max(1) * 20;

    for scope in scopes {
        for (position, entry) in store
            .recall(query, scope, recall_limit)
            .await?
            .into_iter()
            .enumerate()
        {
            let relevance_position = 1.0 / (position as f32 + 1.0);
            let score = entry.confidence * relevance_position;
            ranked.push((score, entry));
        }
    }

    ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut seen = HashSet::new();
    let mut output = String::from("## Memories\n\n");
    let max_chars = token_budget.saturating_mul(4);

    for (_, entry) in ranked {
        let hash = stable_hash(entry.content.as_bytes());
        if !seen.insert(hash) {
            continue;
        }

        let line = format!("- [{:?}/{:?}] {}\n", entry.scope, entry.kind, entry.content);
        if output.len() + line.len() > max_chars && output.len() > "## Memories\n\n".len() {
            break;
        }
        output.push_str(&line);
    }

    Ok(output)
}

pub async fn recall_candidates(
    store: &dyn MemoryStore,
    scopes: &[Scope],
    query: &str,
    max_candidates: usize,
) -> Result<Vec<MemoryEntry>> {
    let mut ranked = Vec::new();
    let recall_limit = scopes.len().max(1) * 20;

    for scope in scopes {
        for (position, entry) in store
            .recall(query, scope, recall_limit)
            .await?
            .into_iter()
            .enumerate()
        {
            let relevance_position = 1.0 / (position as f32 + 1.0);
            let score = entry.confidence * relevance_position;
            ranked.push((score, entry));
        }
    }

    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for (_, entry) in ranked {
        let hash = stable_hash(entry.content.as_bytes());
        if seen.insert(hash) {
            result.push(entry);
            if result.len() >= max_candidates {
                break;
            }
        }
    }
    Ok(result)
}

pub fn format_memories(entries: &[MemoryEntry], token_budget: usize) -> String {
    let mut output = String::from("## Memories\n\n");
    let max_chars = token_budget.saturating_mul(4);

    for entry in entries {
        let line = format!("- [{:?}/{:?}] {}\n", entry.scope, entry.kind, entry.content);
        if output.len() + line.len() > max_chars && output.len() > "## Memories\n\n".len() {
            break;
        }
        output.push_str(&line);
    }
    output
}

fn encode_source(source: &MemorySource) -> String {
    match source {
        MemorySource::UserAuthored => "user_authored".to_string(),
        MemorySource::UserCorrection { session_id } => format!("user_correction:{session_id}"),
        MemorySource::Extracted { session_id } => format!("extracted:{session_id}"),
        MemorySource::Evolution => "evolution".to_string(),
    }
}

fn decode_source(value: &str) -> Result<MemorySource> {
    match value {
        "user_authored" => Ok(MemorySource::UserAuthored),
        "evolution" => Ok(MemorySource::Evolution),
        _ => {
            if let Some(session_id) = value.strip_prefix("user_correction:") {
                return Ok(MemorySource::UserCorrection {
                    session_id: session_id.to_string(),
                });
            }
            if let Some(session_id) = value.strip_prefix("extracted:") {
                return Ok(MemorySource::Extracted {
                    session_id: session_id.to_string(),
                });
            }
            bail!("invalid memory source {value}")
        }
    }
}

fn encode_scope(scope: &Scope) -> String {
    match scope {
        Scope::Global => "global".to_string(),
        Scope::Project(project) => format!("project:{project}"),
        Scope::Agent(agent) => format!("agent:{agent}"),
    }
}

fn decode_scope(value: &str) -> Result<Scope> {
    if value == "global" {
        return Ok(Scope::Global);
    }
    if let Some(project) = value.strip_prefix("project:") {
        return Ok(Scope::Project(project.to_string()));
    }
    if let Some(agent) = value.strip_prefix("agent:") {
        return Ok(Scope::Agent(agent.to_string()));
    }
    bail!("invalid scope {value}")
}

fn encode_kind(kind: &MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Rule => "rule",
        MemoryKind::Preference => "preference",
        MemoryKind::Decision => "decision",
        MemoryKind::Pattern => "pattern",
        MemoryKind::Fact => "fact",
    }
}

fn decode_kind(value: &str) -> Result<MemoryKind> {
    match value {
        "rule" => Ok(MemoryKind::Rule),
        "preference" => Ok(MemoryKind::Preference),
        "decision" => Ok(MemoryKind::Decision),
        "pattern" => Ok(MemoryKind::Pattern),
        "fact" => Ok(MemoryKind::Fact),
        _ => bail!("invalid memory kind {value}"),
    }
}

fn parse_ulid(value: &str) -> Result<Ulid> {
    Ulid::from_str(value).with_context(|| format!("invalid ulid {value}"))
}

fn default_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn project_scope_value(workspace_root: &Path) -> String {
    workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn file_timestamp(path: &Path) -> Result<i64> {
    let modified = fs::metadata(path)?.modified()?;
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!(error))?;
    Ok(duration.as_secs() as i64)
}

fn unix_timestamp() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!(error))?;
    Ok(duration.as_secs() as i64)
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_and_recall_round_trip() -> Result<()> {
        let temp_dir = unique_temp_dir("round-trip");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let entry = sample_entry(
            Scope::Project("workspace".to_string()),
            MemoryKind::Fact,
            "remember the launch checklist",
        )?;

        store.remember(entry.clone()).await?;

        let recalled = store
            .recall("launch", &Scope::Project("workspace".to_string()), 10)
            .await?;

        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].id, entry.id);
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn text_search_returns_relevant_entries() -> Result<()> {
        let temp_dir = unique_temp_dir("text-search");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let scope = Scope::Global;
        let target = sample_entry(scope.clone(), MemoryKind::Rule, "prefer sqlite fts search")?;
        let other = sample_entry(scope.clone(), MemoryKind::Rule, "prefer markdown exports")?;

        store.remember(other).await?;
        store.remember(target.clone()).await?;

        let recalled = store.recall("sqlite", &scope, 10).await?;

        assert!(!recalled.is_empty());
        assert_eq!(recalled[0].id, target.id);
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn recall_with_hyphens_does_not_crash() -> Result<()> {
        let temp_dir = unique_temp_dir("hyphens");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let scope = Scope::Global;
        let entry = sample_entry(
            scope.clone(),
            MemoryKind::Fact,
            "oh-my-openagent is a multi-agent framework",
        )?;

        store.remember(entry.clone()).await?;

        let recalled = store.recall("oh-my-openagent", &scope, 10).await?;
        assert!(!recalled.is_empty());
        assert_eq!(recalled[0].id, entry.id);

        let recalled2 = store.recall("分析一下oh-my-openagent的架构", &scope, 10).await;
        assert!(recalled2.is_ok());
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn reinforce_increments_count() -> Result<()> {
        let temp_dir = unique_temp_dir("reinforce");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let scope = Scope::Agent("planner".to_string());
        let entry = sample_entry(
            scope.clone(),
            MemoryKind::Pattern,
            "reuse existing tool outputs",
        )?;

        store.remember(entry.clone()).await?;
        store.reinforce(entry.id).await?;

        let listed = store.list(&scope, Some(MemoryKind::Pattern)).await?;
        assert_eq!(listed[0].reinforcement_count, 1);
        assert!((listed[0].confidence - 0.55).abs() < f32::EPSILON);
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn forget_removes_entry() -> Result<()> {
        let temp_dir = unique_temp_dir("forget");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let scope = Scope::Global;
        let entry = sample_entry(scope.clone(), MemoryKind::Fact, "temporary memory")?;

        store.remember(entry.clone()).await?;
        store.forget(entry.id).await?;

        let listed = store.list(&scope, None).await?;
        assert!(listed.is_empty());
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn supersede_links_entries() -> Result<()> {
        let temp_dir = unique_temp_dir("supersede");
        let store = MarkdownMemoryStore::open(&temp_dir)?;
        let scope = Scope::Project("workspace".to_string());
        let old = sample_entry(
            scope.clone(),
            MemoryKind::Decision,
            "ship v1 without recall",
        )?;
        let new = sample_entry(scope.clone(), MemoryKind::Decision, "ship v1 with recall")?;

        store.remember(old.clone()).await?;
        store.supersede(old.id, new.clone()).await?;

        let listed = store.list(&scope, Some(MemoryKind::Decision)).await?;
        let stored_old = listed.iter().find(|entry| entry.id == old.id).unwrap();
        let stored_new = listed.iter().find(|entry| entry.id == new.id).unwrap();

        assert_eq!(stored_old.supersedes, Some(new.id));
        assert_eq!(stored_new.supersedes, Some(old.id));
        fs::remove_dir_all(temp_dir)?;
        Ok(())
    }

    #[test]
    fn markdown_parsing_loads_workspace_and_user_memories() -> Result<()> {
        let workspace_root = unique_temp_dir("workspace");
        let home_root = unique_temp_dir("home");
        let loader = MarkdownMemoryLoader::with_home_dir(&workspace_root, &home_root);

        fs::create_dir_all(workspace_root.join(".omh/rules"))?;
        fs::create_dir_all(home_root.join(".omh/memory/global"))?;
        fs::create_dir_all(
            home_root.join(format!(".omh/memory/projects/{}", loader.project_hash())),
        )?;

        fs::write(
            workspace_root.join(".omh/rules.md"),
            "# Ignored\n## Workspace heading\n- Workspace bullet\n",
        )?;
        fs::write(
            workspace_root.join(".omh/rules/extra.md"),
            "- Extra workspace bullet\n",
        )?;
        fs::write(
            home_root.join(".omh/memory/global/global.md"),
            "## Global heading\n",
        )?;
        fs::write(
            home_root.join(format!(
                ".omh/memory/projects/{}/project.md",
                loader.project_hash()
            )),
            "- Project bullet\n",
        )?;

        let mut entries = loader.load()?;
        entries.sort_by(|left, right| left.content.cmp(&right.content));

        assert_eq!(entries.len(), 5);
        assert!(
            entries
                .iter()
                .all(|entry| entry.source == MemorySource::UserAuthored)
        );
        assert!(
            entries
                .iter()
                .all(|entry| (entry.confidence - 1.0).abs() < f32::EPSILON)
        );
        assert!(entries.iter().any(|entry| entry.scope == Scope::Global));
        assert!(entries.iter().any(|entry| {
            entry.scope == Scope::Project(project_scope_value(&workspace_root))
                && entry.content == "Workspace heading"
        }));

        fs::remove_dir_all(workspace_root)?;
        fs::remove_dir_all(home_root)?;
        Ok(())
    }

    fn sample_entry(scope: Scope, kind: MemoryKind, content: &str) -> Result<MemoryEntry> {
        let timestamp = unix_timestamp()?;
        Ok(MemoryEntry {
            id: Ulid::new(),
            scope,
            kind,
            content: content.to_string(),
            source: MemorySource::Extracted {
                session_id: "session-1".to_string(),
            },
            confidence: 0.5,
            reinforcement_count: 0,
            supersedes: None,
            created_at: timestamp,
            updated_at: timestamp,
        })
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("memory-{label}-{}", Ulid::new()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
