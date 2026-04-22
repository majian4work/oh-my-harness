use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPoint {
    PreToolUse,
    PostToolUse,
    PreLlmCall,
    PostLlmCall,
    OnSubagentSpawn,
    OnSubagentComplete,
    OnTurnStart,
    OnTurnEnd,
    OnError,
}

#[derive(Debug, Clone)]
pub struct HookContext {
    pub session_id: String,
    pub agent_name: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_output: Option<String>,
    pub error: Option<String>,
    pub extra: HashMap<String, serde_json::Value>,
}

impl HookContext {
    pub fn new(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_name: agent_name.into(),
            tool_name: None,
            tool_input: None,
            tool_output: None,
            error: None,
            extra: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HookResult {
    Continue,
    Deny(String),
    Abort(String),
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    fn point(&self) -> HookPoint;
    async fn execute(&self, ctx: &mut HookContext) -> HookResult;
}

pub struct HookRunner {
    hooks: Vec<Box<dyn Hook>>,
}

impl HookRunner {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn register(&mut self, hook: Box<dyn Hook>) {
        tracing::debug!(hook = hook.name(), point = ?hook.point(), "hook registered");
        self.hooks.push(hook);
    }

    /// Run all hooks for the given point.
    /// Returns the first non-Continue result, or Continue if all pass.
    pub async fn run(&self, point: HookPoint, ctx: &mut HookContext) -> HookResult {
        for hook in &self.hooks {
            if hook.point() != point {
                continue;
            }
            tracing::debug!(hook = hook.name(), point = ?point, "running hook");
            match hook.execute(ctx).await {
                HookResult::Continue => {}
                result => {
                    tracing::debug!(hook = hook.name(), result = ?result, "hook short-circuited");
                    return result;
                }
            }
        }
        HookResult::Continue
    }

    pub fn hooks_for(&self, point: HookPoint) -> Vec<&str> {
        self.hooks
            .iter()
            .filter(|h| h.point() == point)
            .map(|h| h.name())
            .collect()
    }
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHook {
        result: HookResult,
    }

    #[async_trait]
    impl Hook for TestHook {
        fn name(&self) -> &str {
            "test_hook"
        }
        fn point(&self) -> HookPoint {
            HookPoint::PreToolUse
        }
        async fn execute(&self, _ctx: &mut HookContext) -> HookResult {
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn runner_continues_through_passing_hooks() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(TestHook {
            result: HookResult::Continue,
        }));
        runner.register(Box::new(TestHook {
            result: HookResult::Continue,
        }));

        let mut ctx = HookContext::new("ses_1", "build");
        let result = runner.run(HookPoint::PreToolUse, &mut ctx).await;
        assert!(matches!(result, HookResult::Continue));
    }

    #[tokio::test]
    async fn runner_short_circuits_on_deny() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(TestHook {
            result: HookResult::Deny("blocked".into()),
        }));
        runner.register(Box::new(TestHook {
            result: HookResult::Continue,
        }));

        let mut ctx = HookContext::new("ses_1", "build");
        let result = runner.run(HookPoint::PreToolUse, &mut ctx).await;
        assert!(matches!(result, HookResult::Deny(_)));
    }

    #[tokio::test]
    async fn runner_skips_hooks_for_other_points() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(TestHook {
            result: HookResult::Deny("blocked".into()),
        }));

        let mut ctx = HookContext::new("ses_1", "build");
        // Run PostToolUse — the PreToolUse hook should not fire
        let result = runner.run(HookPoint::PostToolUse, &mut ctx).await;
        assert!(matches!(result, HookResult::Continue));
    }
}
