pub mod builtins;
pub mod parallel;
pub mod truncate;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::anyhow;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PermissionLevel {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub required_permission: PermissionLevel,
    pub supports_parallel: bool,
}

pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub agent_name: String,
    pub workspace_root: PathBuf,
    pub session_dir: Option<PathBuf>,
    pub abort: CancellationToken,
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: HashMap::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: HashMap::new(),
        }
    }
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput>;
}

pub struct ToolRegistry {
    handlers: RwLock<HashMap<String, Arc<dyn ToolHandler>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, handler: Box<dyn ToolHandler>) {
        let name = handler.spec().name.clone();
        self.handlers.write().unwrap().insert(name, Arc::from(handler));
    }

    pub fn get_spec(&self, name: &str) -> Option<ToolSpec> {
        self.handlers.read().unwrap().get(name).map(|h| h.spec())
    }

    pub fn get_handler(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.handlers.read().unwrap().get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.handlers.read().unwrap().values().map(|h| h.spec()).collect()
    }

    pub fn specs_for_permission(&self, max_level: &PermissionLevel) -> Vec<ToolSpec> {
        self.handlers
            .read()
            .unwrap()
            .values()
            .map(|h| h.spec())
            .filter(|s| &s.required_permission <= max_level)
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.handlers.read().unwrap().keys().cloned().collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let handler = {
            let map = self.handlers.read().unwrap();
            Arc::clone(
                map.get(name)
                    .ok_or_else(|| anyhow!("unknown tool: {name}"))?,
            )
        };
        let output = handler.execute(input, ctx).await?;
        let call_id = &ctx.message_id;
        Ok(truncate::maybe_truncate(
            output,
            name,
            call_id,
            ctx.session_dir.as_deref(),
        ))
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn registers_expected_builtin_tools() {
        let registry = ToolRegistry::new();
        builtins::register_builtins(&registry);

        let actual = registry.names().into_iter().collect::<BTreeSet<_>>();
        let expected = [
            "bash",
            "edit_file",
            "glob",
            "grep",
            "read_file",
            "write_file",
        ]
        .into_iter()
        .map(String::from)
        .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
    }
}
