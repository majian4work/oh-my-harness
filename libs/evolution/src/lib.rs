use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use memory::{MemoryEntry, MemoryKind, MemorySource, MemoryStore, Scope};
use message::{ContentPart, Message, Role};
use provider::{CompletionRequest, SystemMessage};
use serde::Deserialize;
use session::Session;
use ulid::Ulid;

pub struct EvolutionEngine {
    memory: Arc<dyn MemoryStore>,
    policy: EvolutionPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvolutionPolicy {
    pub max_rules_per_scope: usize,
    pub min_confidence_to_inject: f32,
    pub require_reinforcement_count: u32,
    pub consolidation_interval: u32,
    pub extraction_model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    pub scope: Scope,
    pub original: String,
    pub corrected: String,
    pub context: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsolidationReport {
    pub merged: usize,
    pub pruned: usize,
    pub strengthened: usize,
}

#[derive(Debug, Clone)]
pub struct PreparedExtraction {
    pub session_id: String,
    pub scope: Scope,
    pub prompt: String,
    pub request: CompletionRequest,
}

impl Default for EvolutionPolicy {
    fn default() -> Self {
        Self {
            max_rules_per_scope: 50,
            min_confidence_to_inject: 0.6,
            require_reinforcement_count: 2,
            consolidation_interval: 10,
            extraction_model: "gpt-4o-mini".to_string(),
        }
    }
}

impl EvolutionEngine {
    pub fn new(memory: Arc<dyn MemoryStore>, policy: EvolutionPolicy) -> Self {
        Self { memory, policy }
    }

    pub fn learn_from_correction(&self, correction: Correction) -> Result<MemoryEntry> {
        let now = unix_timestamp()?;
        let entry = MemoryEntry {
            id: Ulid::new(),
            scope: correction.scope.clone(),
            kind: MemoryKind::Rule,
            content: format!(
                "Use `{}` instead of `{}`. Context: {}",
                correction.corrected.trim(),
                correction.original.trim(),
                correction.context.trim()
            ),
            source: MemorySource::UserCorrection {
                session_id: correction.session_id,
            },
            confidence: 0.9,
            reinforcement_count: 0,
            supersedes: None,
            created_at: now,
            updated_at: now,
        };

        let memory = Arc::clone(&self.memory);
        let stored = entry.clone();
        run_async(async move {
            memory.remember(stored).await?;
            Ok(())
        })?;

        Ok(entry)
    }

    pub fn prepare_extraction_request(
        &self,
        messages: &[Message],
        session_id: &str,
        scope: &Scope,
    ) -> PreparedExtraction {
        let prompt = build_extraction_prompt(messages, session_id, scope);
        let request = CompletionRequest {
            model: self.policy.extraction_model.clone(),
            system: vec![SystemMessage {
                content: extraction_system_prompt().to_string(),
                cache_control: false,
            }],
            messages: vec![Message::user(
                format!("extract-{session_id}"),
                prompt.clone(),
            )],
            tools: Vec::new(),
            temperature: Some(0.0),
            max_tokens: Some(800),
        };

        PreparedExtraction {
            session_id: session_id.to_string(),
            scope: scope.clone(),
            prompt,
            request,
        }
    }

    pub fn prepare_session_extraction(
        &self,
        session: &Session,
        scope: &Scope,
    ) -> PreparedExtraction {
        self.prepare_extraction_request(&session.messages, &session.id, scope)
    }

    pub fn extract_learnings(
        &self,
        messages: &[Message],
        session_id: &str,
        scope: &Scope,
    ) -> Result<Vec<MemoryEntry>> {
        let prepared = self.prepare_extraction_request(messages, session_id, scope);
        tracing::debug!(
            session_id = prepared.session_id,
            model = prepared.request.model,
            prompt_len = prepared.prompt.len(),
            "prepared evolution extraction request"
        );

        let Some(response) = messages.iter().rev().find_map(extraction_json_candidate) else {
            return Ok(Vec::new());
        };

        self.parse_extraction_response(&response, session_id, scope)
    }

    pub fn parse_extraction_response(
        &self,
        response: &str,
        session_id: &str,
        scope: &Scope,
    ) -> Result<Vec<MemoryEntry>> {
        let parsed: ExtractionResponse = serde_json::from_str(response)
            .with_context(|| "failed to parse extraction response JSON".to_string())?;
        let learnings = match parsed {
            ExtractionResponse::Items(items) => items,
            ExtractionResponse::Envelope { learnings } => learnings,
        };

        let now = unix_timestamp()?;
        let mut entries = Vec::new();
        for learning in learnings {
            let content = learning.content.trim();
            if content.is_empty() {
                continue;
            }

            entries.push(MemoryEntry {
                id: Ulid::new(),
                scope: scope.clone(),
                kind: parse_memory_kind(learning.kind.as_deref().unwrap_or("rule"))?,
                content: content.to_string(),
                source: MemorySource::Extracted {
                    session_id: session_id.to_string(),
                },
                confidence: 0.5,
                reinforcement_count: 0,
                supersedes: None,
                created_at: now,
                updated_at: now,
            });
        }

        Ok(entries)
    }

    pub fn consolidate(&self, scope: &Scope) -> Result<ConsolidationReport> {
        let memory = Arc::clone(&self.memory);
        let list_scope = scope.clone();
        let mut entries = run_async(async move { memory.list(&list_scope, None).await })?;
        entries.sort_by(|left, right| {
            right
                .content
                .len()
                .cmp(&left.content.len())
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });

        let mut report = ConsolidationReport::default();
        let mut consumed = vec![false; entries.len()];

        for index in 0..entries.len() {
            if consumed[index] {
                continue;
            }

            let mut cluster = vec![index];
            for other in (index + 1)..entries.len() {
                if consumed[other] {
                    continue;
                }
                if entries[index].kind != entries[other].kind {
                    continue;
                }
                if contents_overlap(&entries[index].content, &entries[other].content) {
                    cluster.push(other);
                }
            }

            if cluster.len() == 1 {
                continue;
            }

            for &cluster_index in &cluster {
                consumed[cluster_index] = true;
            }

            let grouped = cluster
                .iter()
                .map(|&cluster_index| entries[cluster_index].clone())
                .collect::<Vec<_>>();
            let strengthened = grouped
                .iter()
                .max_by(|left, right| compare_confidence_then_reinforcement(left, right))
                .map(|best| {
                    grouped
                        .iter()
                        .map(|entry| entry.reinforcement_count)
                        .sum::<u32>()
                        > best.reinforcement_count
                })
                .unwrap_or(false);
            let merged = build_merged_entry(scope, &grouped)?;
            let forget_ids = grouped.iter().map(|entry| entry.id).collect::<Vec<_>>();

            let memory = Arc::clone(&self.memory);
            let entry = merged.clone();
            run_async(async move {
                memory.remember(entry).await?;
                for id in forget_ids {
                    memory.forget(id).await?;
                }
                Ok(())
            })?;

            report.merged += grouped.len() - 1;
            if strengthened {
                report.strengthened += 1;
            }
        }

        let prunable = entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                !consumed[*index] && entry.confidence < 0.3 && entry.reinforcement_count == 0
            })
            .map(|(_, entry)| entry.id)
            .collect::<Vec<_>>();

        if !prunable.is_empty() {
            let memory = Arc::clone(&self.memory);
            let pruned = prunable.len();
            run_async(async move {
                for id in prunable {
                    memory.forget(id).await?;
                }
                Ok(())
            })?;
            report.pruned += pruned;
        }

        Ok(report)
    }

    pub fn reinforce(&self, id: Ulid) -> Result<()> {
        let memory = Arc::clone(&self.memory);
        run_async(async move {
            memory.reinforce(id).await?;
            Ok(())
        })
    }

    pub fn should_inject(&self, entry: &MemoryEntry) -> bool {
        matches!(
            entry.source,
            MemorySource::UserAuthored | MemorySource::UserCorrection { .. }
        ) || (entry.confidence >= self.policy.min_confidence_to_inject
            && entry.reinforcement_count >= self.policy.require_reinforcement_count)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ExtractionResponse {
    Items(Vec<ExtractedLearning>),
    Envelope { learnings: Vec<ExtractedLearning> },
}

#[derive(Debug, Deserialize)]
struct ExtractedLearning {
    content: String,
    #[serde(default)]
    kind: Option<String>,
}

fn extraction_system_prompt() -> &'static str {
    "Extract durable learnings from the session. Return only JSON as either an array or an object with a 'learnings' array. Each learning must contain {\"kind\":\"rule|preference|decision|pattern|fact\",\"content\":\"...\"}. Only include reusable guidance."
}

fn build_extraction_prompt(messages: &[Message], session_id: &str, scope: &Scope) -> String {
    let transcript = messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "Session: {session_id}\nScope: {:?}\n\nTranscript:\n{}\n",
        scope, transcript
    )
}

fn render_message(message: &Message) -> String {
    let label = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };

    let body = message
        .parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.clone(),
            ContentPart::Thinking { text } => format!("[thinking] {text}"),
            ContentPart::ToolUse { id, name, input } => {
                format!("[tool_use:{id}:{name}] {input}")
            }
            ContentPart::ToolResult {
                id,
                content,
                is_error,
            } => format!("[tool_result:{id}:error={is_error}] {content}"),
            ContentPart::Image { media_type, .. } => format!("[image:{media_type} omitted]"),
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("{label}: {body}")
}

fn extraction_json_candidate(message: &Message) -> Option<String> {
    if !matches!(message.role, Role::Assistant) {
        return None;
    }

    let text = message.text();
    let trimmed = text.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return Some(trimmed.to_string());
    }
    None
}

fn parse_memory_kind(value: &str) -> Result<MemoryKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "rule" => Ok(MemoryKind::Rule),
        "preference" => Ok(MemoryKind::Preference),
        "decision" => Ok(MemoryKind::Decision),
        "pattern" => Ok(MemoryKind::Pattern),
        "fact" => Ok(MemoryKind::Fact),
        other => bail!("invalid memory kind {other}"),
    }
}

fn build_merged_entry(scope: &Scope, entries: &[MemoryEntry]) -> Result<MemoryEntry> {
    let Some(best) = entries
        .iter()
        .max_by(|left, right| compare_confidence_then_reinforcement(left, right))
    else {
        bail!("cannot merge empty entry set");
    };

    let content = entries
        .iter()
        .max_by_key(|entry| entry.content.len())
        .map(|entry| entry.content.clone())
        .unwrap_or_default();
    let confidence = entries
        .iter()
        .map(|entry| entry.confidence)
        .fold(0.0_f32, f32::max);
    let reinforcement_count = entries.iter().map(|entry| entry.reinforcement_count).sum();
    let created_at = entries
        .iter()
        .map(|entry| entry.created_at)
        .min()
        .unwrap_or(unix_timestamp()?);
    let updated_at = unix_timestamp()?;

    Ok(MemoryEntry {
        id: Ulid::new(),
        scope: scope.clone(),
        kind: best.kind.clone(),
        content,
        source: best.source.clone(),
        confidence,
        reinforcement_count,
        supersedes: Some(best.id),
        created_at,
        updated_at,
    })
}

fn compare_confidence_then_reinforcement(
    left: &MemoryEntry,
    right: &MemoryEntry,
) -> std::cmp::Ordering {
    left.confidence
        .partial_cmp(&right.confidence)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.reinforcement_count.cmp(&right.reinforcement_count))
        .then_with(|| left.content.len().cmp(&right.content.len()))
}

fn contents_overlap(left: &str, right: &str) -> bool {
    let left = normalize_content(left);
    let right = normalize_content(right);
    !left.is_empty() && !right.is_empty() && (left.contains(&right) || right.contains(&left))
}

fn normalize_content(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn run_async<T, F>(future: F) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow!(error))
                .and_then(|runtime| runtime.block_on(future));
            let _ = sender.send(result);
        });

        return receiver
            .recv()
            .map_err(|_| anyhow!("async worker thread terminated"))?;
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| anyhow!(error))?
        .block_on(future)
}

fn unix_timestamp() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!(error))?;
    Ok(duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    #[test]
    fn learn_from_correction_creates_proper_entry() -> Result<()> {
        let store = Arc::new(MockMemoryStore::default());
        let engine = EvolutionEngine::new(store.clone(), EvolutionPolicy::default());
        let correction = Correction {
            scope: Scope::Project("workspace".to_string()),
            original: "write large refactors".to_string(),
            corrected: "make surgical changes".to_string(),
            context: "The user asked for a targeted fix".to_string(),
            session_id: "ses_123".to_string(),
        };

        let entry = engine.learn_from_correction(correction)?;

        assert_eq!(entry.kind, MemoryKind::Rule);
        assert_eq!(entry.scope, Scope::Project("workspace".to_string()));
        assert!((entry.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(entry.reinforcement_count, 0);
        assert!(entry.content.contains("make surgical changes"));
        assert!(entry.content.contains("write large refactors"));
        assert_eq!(
            entry.source,
            MemorySource::UserCorrection {
                session_id: "ses_123".to_string()
            }
        );
        assert_eq!(store.entries().len(), 1);
        Ok(())
    }

    #[test]
    fn should_inject_logic_handles_expected_cases() {
        let engine = EvolutionEngine::new(
            Arc::new(MockMemoryStore::default()),
            EvolutionPolicy::default(),
        );

        assert!(engine.should_inject(&sample_entry(
            MemorySource::UserAuthored,
            0.1,
            0,
            "always keep user-authored rules",
        )));
        assert!(engine.should_inject(&sample_entry(
            MemorySource::UserCorrection {
                session_id: "ses_1".to_string(),
            },
            0.1,
            0,
            "corrections always inject",
        )));
        assert!(engine.should_inject(&sample_entry(
            MemorySource::Extracted {
                session_id: "ses_2".to_string(),
            },
            0.7,
            2,
            "reinforced and confident",
        )));
        assert!(!engine.should_inject(&sample_entry(
            MemorySource::Extracted {
                session_id: "ses_2".to_string(),
            },
            0.7,
            1,
            "not enough reinforcement",
        )));
        assert!(!engine.should_inject(&sample_entry(
            MemorySource::Extracted {
                session_id: "ses_2".to_string(),
            },
            0.5,
            2,
            "not enough confidence",
        )));
    }

    #[test]
    fn parse_extraction_response_parses_valid_json() -> Result<()> {
        let engine = EvolutionEngine::new(
            Arc::new(MockMemoryStore::default()),
            EvolutionPolicy::default(),
        );
        let response = r#"
        {
          "learnings": [
            {"kind": "rule", "content": "Prefer small focused diffs"},
            {"kind": "pattern", "content": "Users value direct execution"}
          ]
        }
        "#;

        let entries = engine.parse_extraction_response(response, "ses_extract", &Scope::Global)?;

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, MemoryKind::Rule);
        assert_eq!(entries[1].kind, MemoryKind::Pattern);
        assert!(entries.iter().all(|entry| entry.scope == Scope::Global));
        assert!(
            entries
                .iter()
                .all(|entry| (entry.confidence - 0.5).abs() < f32::EPSILON)
        );
        assert!(entries.iter().all(|entry| {
            entry.source
                == MemorySource::Extracted {
                    session_id: "ses_extract".to_string(),
                }
        }));
        Ok(())
    }

    #[test]
    fn consolidate_merges_duplicates_and_prunes_low_confidence() -> Result<()> {
        let store = Arc::new(MockMemoryStore::default());
        let scope = Scope::Global;
        store.seed(vec![
            scoped_entry(
                scope.clone(),
                MemoryKind::Rule,
                "prefer concise commit messages",
                MemorySource::Extracted {
                    session_id: "ses_a".to_string(),
                },
                0.7,
                1,
            ),
            scoped_entry(
                scope.clone(),
                MemoryKind::Rule,
                "concise commit messages",
                MemorySource::UserCorrection {
                    session_id: "ses_b".to_string(),
                },
                0.9,
                2,
            ),
            scoped_entry(
                scope.clone(),
                MemoryKind::Fact,
                "temporary note",
                MemorySource::Extracted {
                    session_id: "ses_c".to_string(),
                },
                0.2,
                0,
            ),
            scoped_entry(
                scope.clone(),
                MemoryKind::Rule,
                "keep tests focused",
                MemorySource::Extracted {
                    session_id: "ses_d".to_string(),
                },
                0.8,
                2,
            ),
        ]);
        let engine = EvolutionEngine::new(store.clone(), EvolutionPolicy::default());

        let report = engine.consolidate(&scope)?;
        let listed = store.list_sync(&scope, None);

        assert_eq!(report.merged, 1);
        assert_eq!(report.pruned, 1);
        assert_eq!(report.strengthened, 1);
        assert_eq!(listed.len(), 2);
        let merged = listed
            .iter()
            .find(|entry| entry.content == "prefer concise commit messages")
            .unwrap();
        assert!((merged.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(merged.reinforcement_count, 3);
        assert_eq!(
            merged.source,
            MemorySource::UserCorrection {
                session_id: "ses_b".to_string()
            }
        );
        assert!(
            listed
                .iter()
                .any(|entry| entry.content == "keep tests focused")
        );
        Ok(())
    }

    #[test]
    fn default_policy_has_expected_values() {
        let policy = EvolutionPolicy::default();

        assert_eq!(policy.max_rules_per_scope, 50);
        assert!((policy.min_confidence_to_inject - 0.6).abs() < f32::EPSILON);
        assert_eq!(policy.require_reinforcement_count, 2);
        assert_eq!(policy.consolidation_interval, 10);
        assert_eq!(policy.extraction_model, "gpt-4o-mini");
    }

    #[derive(Default)]
    struct MockMemoryStore {
        entries: Mutex<Vec<MemoryEntry>>,
    }

    impl MockMemoryStore {
        fn seed(&self, entries: Vec<MemoryEntry>) {
            *self.entries.lock().unwrap() = entries;
        }

        fn entries(&self) -> Vec<MemoryEntry> {
            self.entries.lock().unwrap().clone()
        }

        fn list_sync(&self, scope: &Scope, kind: Option<MemoryKind>) -> Vec<MemoryEntry> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .filter(|entry| {
                    &entry.scope == scope && kind.as_ref().is_none_or(|value| &entry.kind == value)
                })
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn remember(&self, entry: MemoryEntry) -> Result<()> {
            self.entries.lock().unwrap().push(entry);
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _scope: &Scope,
            _limit: usize,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, id: Ulid) -> Result<()> {
            let mut entries = self.entries.lock().unwrap();
            let before = entries.len();
            entries.retain(|entry| entry.id != id);
            if entries.len() == before {
                bail!("memory entry {id} not found")
            }
            Ok(())
        }

        async fn supersede(&self, old_id: Ulid, new: MemoryEntry) -> Result<()> {
            let mut entries = self.entries.lock().unwrap();
            let Some(old) = entries.iter_mut().find(|entry| entry.id == old_id) else {
                bail!("memory entry {old_id} not found")
            };
            old.supersedes = Some(new.id);
            entries.push(new);
            Ok(())
        }

        async fn reinforce(&self, id: Ulid) -> Result<()> {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                bail!("memory entry {id} not found")
            };
            entry.reinforcement_count += 1;
            entry.confidence = (entry.confidence + 0.05).min(1.0);
            Ok(())
        }

        async fn list(&self, scope: &Scope, kind: Option<MemoryKind>) -> Result<Vec<MemoryEntry>> {
            Ok(self.list_sync(scope, kind))
        }
    }

    fn sample_entry(
        source: MemorySource,
        confidence: f32,
        reinforcement_count: u32,
        content: &str,
    ) -> MemoryEntry {
        scoped_entry(
            Scope::Global,
            MemoryKind::Rule,
            content,
            source,
            confidence,
            reinforcement_count,
        )
    }

    fn scoped_entry(
        scope: Scope,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        confidence: f32,
        reinforcement_count: u32,
    ) -> MemoryEntry {
        MemoryEntry {
            id: Ulid::new(),
            scope,
            kind,
            content: content.to_string(),
            source,
            confidence,
            reinforcement_count,
            supersedes: None,
            created_at: 100,
            updated_at: 100,
        }
    }
}
