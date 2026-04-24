pub mod anthropic;
pub mod copilot;
pub mod mock;
pub mod openai_common;
pub mod openai_compat;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use message::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCostTier {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model_id: String,
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage {
    pub content: String,
    pub cache_control: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub system: Vec<SystemMessage>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, args_chunk: String },
    ToolCallEnd { id: String },
    Usage(UsageStats),
    Done,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub message: Message,
    pub usage: UsageStats,
}

pub type StreamResult = Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> String;
    async fn stream(&self, request: CompletionRequest) -> anyhow::Result<StreamResult>;
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse>;

    fn model_for_tier(&self, tier: ModelCostTier) -> String {
        let default = self.default_model();
        let cost = infer_model_cost(&default);
        if cost == tier {
            return default;
        }
        default
    }

    fn context_window(&self, model: &str) -> usize {
        infer_context_window(model)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(vec![])
    }

    /// Validate that a model is actually accessible via chat/completions.
    /// Sends a minimal request (max_tokens=1). Returns true if the model responds successfully.
    async fn validate_model(&self, model_id: &str) -> bool {
        let request = CompletionRequest {
            model: model_id.to_string(),
            system: vec![],
            messages: vec![Message::user("validate", "hi")],
            tools: vec![],
            max_tokens: Some(1),
            temperature: None,
        };
        self.complete(request).await.is_ok()
    }

    /// List models with validation — only returns models that pass validate_model.
    /// Runs validation in parallel with concurrency limit.
    async fn list_models_validated(&self) -> anyhow::Result<Vec<ModelInfo>> {
        let all = self.list_models().await?;
        if all.is_empty() {
            return Ok(all);
        }

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(5));
        let mut handles = Vec::new();

        for model in all {
            let sem = semaphore.clone();
            let model_id = model.id.clone();
            handles.push(async move {
                let _permit = sem.acquire().await;
                let valid = self.validate_model(&model_id).await;
                (model, valid)
            });
        }

        let results = futures::future::join_all(handles).await;
        Ok(results
            .into_iter()
            .filter(|(_, valid)| *valid)
            .map(|(m, _)| m)
            .collect())
    }
}

pub struct ProviderRegistry {
    providers: std::collections::HashMap<String, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: std::collections::HashMap::new(),
        }
    }

    pub fn register(&mut self, id: impl Into<String>, provider: Box<dyn Provider>) {
        self.providers.insert(id.into(), provider);
    }

    pub fn get(&self, id: &str) -> Option<&dyn Provider> {
        self.providers.get(id).map(|p| p.as_ref())
    }

    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|k| k.as_str()).collect()
    }

    pub fn first_id(&self) -> Option<&str> {
        self.providers.keys().next().map(|k| k.as_str())
    }

    pub fn resolve_model(
        &self,
        requested: Option<&ModelSpec>,
        cost_hint: Option<ModelCostTier>,
    ) -> Option<ResolvedModel> {
        tracing::trace!(
            requested = ?requested.map(|s| &s.model_id),
            provider_hint = ?requested.and_then(|s| s.provider_id.as_deref()),
            cost_hint = ?cost_hint,
            "resolve_model entry"
        );

        if let Some(spec) = requested {
            if let Some(pid) = spec.provider_id.as_deref() {
                if self.get(pid).is_some() {
                    tracing::trace!(
                        model = %spec.model_id,
                        provider = %pid,
                        "resolve_model: exact provider match"
                    );
                    return Some(ResolvedModel {
                        provider_id: pid.to_string(),
                        model_id: spec.model_id.clone(),
                    });
                }
            }

            let (pid, _) = self.providers.iter().next()?;
            tracing::trace!(
                model = %spec.model_id,
                provider = %pid,
                "resolve_model: using requested model as-is"
            );
            return Some(ResolvedModel {
                provider_id: pid.clone(),
                model_id: spec.model_id.clone(),
            });
        }

        let tier = cost_hint.unwrap_or(ModelCostTier::Medium);
        let (pid, provider) = self.providers.iter().next()?;
        let model_id = provider.model_for_tier(tier);
        tracing::trace!(
            tier = ?tier,
            resolved = %model_id,
            provider = %pid,
            "resolve_model: no model requested, using tier default"
        );
        Some(ResolvedModel {
            provider_id: pid.clone(),
            model_id,
        })
    }

    pub async fn list_all_models(&self) -> Vec<(String, Vec<ModelInfo>)> {
        let mut result = Vec::new();
        for (id, provider) in &self.providers {
            let models = provider.list_models().await.unwrap_or_default();
            if !models.is_empty() {
                result.push((id.clone(), models));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    pub async fn list_all_models_validated(&self) -> Vec<(String, Vec<ModelInfo>)> {
        let mut result = Vec::new();
        for (id, provider) in &self.providers {
            let models = provider.list_models_validated().await.unwrap_or_default();
            if !models.is_empty() {
                result.push((id.clone(), models));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn infer_model_cost(model_id: &str) -> ModelCostTier {
    let id = model_id.to_lowercase();
    if id.contains("mini") || id.contains("flash") || id.contains("haiku") || id.contains("nano") {
        ModelCostTier::Low
    } else if id.contains("o3") || id.contains("o1") || id.contains("opus") || id.contains("ultra")
    {
        ModelCostTier::High
    } else {
        ModelCostTier::Medium
    }
}

pub fn infer_context_window(model_id: &str) -> usize {
    let id = model_id.to_lowercase();
    if id.contains("claude") {
        200_000
    } else if id.contains("gpt-4.1")
        || id.contains("gpt-5")
        || id.contains("o3")
        || id.contains("o4")
    {
        1_000_000
    } else if id.contains("gpt-4o") {
        128_000
    } else if id.contains("gemini") {
        1_000_000
    } else {
        128_000
    }
}
