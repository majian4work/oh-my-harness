use std::sync::Arc;

use tokio::sync::RwLock;

use crate::{ToolContext, ToolOutput, ToolRegistry};

pub struct ToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

pub struct ParallelExecutor {
    registry: Arc<ToolRegistry>,
    lock: Arc<RwLock<()>>,
}

impl ParallelExecutor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            lock: Arc::new(RwLock::new(())),
        }
    }

    pub async fn execute_batch(
        &self,
        calls: Vec<ToolCall>,
        ctx: &ToolContext,
    ) -> Vec<(String, anyhow::Result<ToolOutput>)> {
        let mut handles = Vec::new();

        for call in calls {
            let registry = self.registry.clone();
            let lock = self.lock.clone();
            let session_id = ctx.session_id.clone();
            let message_id = ctx.message_id.clone();
            let agent_name = ctx.agent_name.clone();
            let workspace_root = ctx.workspace_root.clone();
            let abort = ctx.abort.clone();
            let depth = ctx.depth;

            handles.push(tokio::spawn(async move {
                let handler = match registry.get_handler(&call.tool_name) {
                    Some(handler) => handler,
                    None => {
                        return (
                            call.call_id,
                            Err(anyhow::anyhow!("unknown tool: {}", call.tool_name)),
                        );
                    }
                };

                let supports_parallel = handler.spec().supports_parallel;
                let ctx = ToolContext {
                    session_id,
                    message_id,
                    agent_name,
                    workspace_root,
                    session_dir: None,
                    abort,
                    depth,
                };

                let result = if supports_parallel {
                    let _guard = lock.read().await;
                    handler.execute(call.input, &ctx).await
                } else {
                    let _guard = lock.write().await;
                    handler.execute(call.input, &ctx).await
                };

                (call.call_id, result)
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(error) => results.push((
                    String::new(),
                    Err(anyhow::anyhow!("task join error: {error}")),
                )),
            }
        }

        results
    }
}
