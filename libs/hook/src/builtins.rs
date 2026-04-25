use agent::AgentRegistry;
use async_trait::async_trait;
use bus::{ApprovalChannel, ApprovalResponse};
use permission::PermissionDecision;
use tool::ToolRegistry;

use std::sync::Arc;

use crate::{Hook, HookContext, HookPoint, HookResult};

/// Enforces the current agent's permission policy on tool calls.
/// Looks up the agent by name from the registry to get its permission rules.
pub struct PermissionGuardHook {
    agent_registry: AgentRegistry,
    tool_registry: Arc<ToolRegistry>,
    approval_channel: ApprovalChannel,
}

impl PermissionGuardHook {
    pub fn new(
        agent_registry: AgentRegistry,
        tool_registry: Arc<ToolRegistry>,
        approval_channel: ApprovalChannel,
    ) -> Self {
        Self {
            agent_registry,
            tool_registry,
            approval_channel,
        }
    }
}

#[async_trait]
impl Hook for PermissionGuardHook {
    fn name(&self) -> &str {
        "permission_guard"
    }

    fn point(&self) -> HookPoint {
        HookPoint::PreToolUse
    }

    async fn execute(&self, ctx: &mut HookContext) -> HookResult {
        let Some(tool_name) = &ctx.tool_name else {
            return HookResult::Continue;
        };

        let Some(agent) = self.agent_registry.get(&ctx.agent_name) else {
            return HookResult::Continue; // unknown agent — skip
        };

        let spec = match self.tool_registry.get_spec(tool_name) {
            Some(s) => s,
            None => return HookResult::Continue, // unknown tool — let execution handle it
        };

        let input = ctx.tool_input.as_ref().cloned().unwrap_or_default();

        match agent
            .permission_rules
            .evaluate(tool_name, &spec.required_permission, &input)
        {
            PermissionDecision::Allow => HookResult::Continue,
            PermissionDecision::Deny(reason) => {
                tracing::warn!(
                    tool = %tool_name,
                    agent = %ctx.agent_name,
                    reason = %reason,
                    "permission guard denied tool call"
                );
                HookResult::Deny(reason)
            }
            PermissionDecision::Ask(reason) => {
                tracing::info!(
                    tool = %tool_name,
                    agent = %ctx.agent_name,
                    reason = %reason,
                    "permission guard: tool requires user approval"
                );
                let response = self
                    .approval_channel
                    .request_approval(
                        ctx.session_id.clone(),
                        tool_name.clone(),
                        input,
                        reason.clone(),
                    )
                    .await;
                match response {
                    ApprovalResponse::Allow => {
                        tracing::info!(tool = %tool_name, "user approved tool execution");
                        HookResult::Continue
                    }
                    ApprovalResponse::Deny => {
                        tracing::info!(tool = %tool_name, "user denied tool execution");
                        HookResult::Deny(format!("User denied: {reason}"))
                    }
                }
            }
        }
    }
}

/// Emits structured tracing events at each hook point for observability.
pub struct AuditTrailHook {
    point: HookPoint,
}

impl AuditTrailHook {
    pub fn new(point: HookPoint) -> Self {
        Self { point }
    }

    /// Create one AuditTrailHook per hook point.
    pub fn all() -> Vec<Self> {
        vec![
            Self::new(HookPoint::PreToolUse),
            Self::new(HookPoint::PostToolUse),
            Self::new(HookPoint::PreLlmCall),
            Self::new(HookPoint::PostLlmCall),
            Self::new(HookPoint::OnSubagentSpawn),
            Self::new(HookPoint::OnSubagentComplete),
            Self::new(HookPoint::OnTurnStart),
            Self::new(HookPoint::OnTurnEnd),
            Self::new(HookPoint::OnError),
        ]
    }
}

#[async_trait]
impl Hook for AuditTrailHook {
    fn name(&self) -> &str {
        "audit_trail"
    }

    fn point(&self) -> HookPoint {
        self.point
    }

    async fn execute(&self, ctx: &mut HookContext) -> HookResult {
        match self.point {
            HookPoint::PreToolUse | HookPoint::PostToolUse => {
                tracing::debug!(
                    hook_point = ?self.point,
                    session = %ctx.session_id,
                    agent = %ctx.agent_name,
                    tool = ?ctx.tool_name,
                    "audit"
                );
            }
            HookPoint::OnError => {
                tracing::warn!(
                    hook_point = ?self.point,
                    session = %ctx.session_id,
                    agent = %ctx.agent_name,
                    error = ?ctx.error,
                    "audit"
                );
            }
            _ => {
                tracing::debug!(
                    hook_point = ?self.point,
                    session = %ctx.session_id,
                    agent = %ctx.agent_name,
                    "audit"
                );
            }
        }
        HookResult::Continue
    }
}

/// Tracks consecutive tool errors within a turn and aborts if too many accumulate.
pub struct ErrorTrackerHook {
    max_consecutive_errors: u32,
}

impl ErrorTrackerHook {
    pub fn new(max_consecutive_errors: u32) -> Self {
        Self {
            max_consecutive_errors,
        }
    }
}

const ERROR_COUNT_KEY: &str = "consecutive_tool_errors";

#[async_trait]
impl Hook for ErrorTrackerHook {
    fn name(&self) -> &str {
        "error_tracker"
    }

    fn point(&self) -> HookPoint {
        HookPoint::PostToolUse
    }

    async fn execute(&self, ctx: &mut HookContext) -> HookResult {
        let count = ctx
            .extra
            .get(ERROR_COUNT_KEY)
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        if ctx.error.is_some() {
            let new_count = count + 1;
            ctx.extra.insert(
                ERROR_COUNT_KEY.to_string(),
                serde_json::Value::Number(new_count.into()),
            );
            if new_count >= self.max_consecutive_errors {
                tracing::warn!(
                    consecutive_errors = new_count,
                    agent = %ctx.agent_name,
                    tool = ?ctx.tool_name,
                    "too many consecutive tool errors, aborting"
                );
                return HookResult::Abort(format!(
                    "{new_count} consecutive tool errors — aborting to avoid wasting tokens"
                ));
            }
        } else {
            // Reset on success
            ctx.extra.insert(
                ERROR_COUNT_KEY.to_string(),
                serde_json::Value::Number(0.into()),
            );
        }

        HookResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bus::ApprovalChannel;
    use permission::{PermissionPolicy, PermissionRule};
    use tool::PermissionLevel;

    fn mock_tool_registry() -> Arc<ToolRegistry> {
        let registry = ToolRegistry::new();
        struct BashMock;
        #[async_trait]
        impl tool::ToolHandler for BashMock {
            fn spec(&self) -> tool::ToolSpec {
                tool::ToolSpec {
                    name: "bash".to_string(),
                    description: "shell".to_string(),
                    input_schema: serde_json::json!({}),
                    required_permission: PermissionLevel::FullAccess,
                    supports_parallel: false,
                }
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &tool::ToolContext,
            ) -> anyhow::Result<tool::ToolOutput> {
                Ok(tool::ToolOutput::text("ok"))
            }
        }
        registry.register(Box::new(BashMock));
        Arc::new(registry)
    }

    /// Builds an AgentRegistry with a test agent having the given permission policy.
    fn test_registry_with_policy(agent_name: &str, policy: PermissionPolicy) -> AgentRegistry {
        let level = match policy.default_level {
            PermissionLevel::ReadOnly => "ReadOnly",
            PermissionLevel::WorkspaceWrite => "WorkspaceWrite",
            PermissionLevel::FullAccess => "FullAccess",
        };
        let deny_lines: String = policy
            .deny_rules
            .iter()
            .map(|r| format!("- deny: {}\n", r.tool_pattern))
            .collect();
        let permissions_section = if deny_lines.is_empty() {
            String::new()
        } else {
            format!(
                "permissions:\n{}",
                policy
                    .deny_rules
                    .iter()
                    .map(|r| format!("  deny: {}\n", r.tool_pattern))
                    .collect::<String>()
            )
        };
        let md = format!(
            "---\n\
             name: {agent_name}\n\
             config:\n\
             \x20 mode: primary\n\
             \x20 cost: free\n\
             \x20 permission_level: {level}\n\
             {permissions_section}\
             ---\n\
             test"
        );
        let dir =
            std::env::temp_dir().join(format!("hook-test-{}-{agent_name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(format!("{agent_name}.md")), &md).unwrap();
        let reg = AgentRegistry::load_from_paths(vec![dir.clone()]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        reg
    }

    #[tokio::test]
    async fn permission_guard_allows_permitted_tool() {
        let reg = test_registry_with_policy("test-agent", PermissionPolicy::permissive());
        let hook = PermissionGuardHook::new(reg, mock_tool_registry(), ApprovalChannel::new());
        let mut ctx = HookContext::new("s1", "test-agent");
        ctx.tool_name = Some("bash".to_string());
        ctx.tool_input = Some(serde_json::json!({"command": "ls"}));

        let result = hook.execute(&mut ctx).await;
        assert!(matches!(result, HookResult::Continue));
    }

    #[tokio::test]
    async fn permission_guard_denies_read_only_for_bash() {
        let channel = ApprovalChannel::new();
        let reg = test_registry_with_policy("ro-agent", PermissionPolicy::read_only());
        let hook = PermissionGuardHook::new(reg, mock_tool_registry(), channel.clone());
        let mut ctx = HookContext::new("s1", "ro-agent");
        ctx.tool_name = Some("bash".to_string());
        ctx.tool_input = Some(serde_json::json!({}));

        // Spawn a responder that denies the approval request
        tokio::spawn(async move {
            loop {
                if let Some(req) = channel.try_recv().await {
                    let _ = req.respond.send(ApprovalResponse::Deny);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let result = hook.execute(&mut ctx).await;
        assert!(matches!(result, HookResult::Deny(_)));
    }

    #[tokio::test]
    async fn permission_guard_skips_unknown_agent() {
        let paths: Vec<std::path::PathBuf> = vec![];
        let reg = AgentRegistry::load_from_paths(paths).unwrap();
        let hook = PermissionGuardHook::new(reg, mock_tool_registry(), ApprovalChannel::new());
        let mut ctx = HookContext::new("s1", "nonexistent-agent");
        ctx.tool_name = Some("bash".to_string());

        let result = hook.execute(&mut ctx).await;
        assert!(matches!(result, HookResult::Continue));
    }

    #[tokio::test]
    async fn error_tracker_aborts_after_threshold() {
        let hook = ErrorTrackerHook::new(3);
        let mut ctx = HookContext::new("s1", "agent");
        ctx.tool_name = Some("bash".to_string());
        ctx.error = Some("fail".to_string());

        // 1st error
        let r = hook.execute(&mut ctx).await;
        assert!(matches!(r, HookResult::Continue));

        // 2nd error
        let r = hook.execute(&mut ctx).await;
        assert!(matches!(r, HookResult::Continue));

        // 3rd error — should abort
        let r = hook.execute(&mut ctx).await;
        assert!(matches!(r, HookResult::Abort(_)));
    }

    #[tokio::test]
    async fn error_tracker_resets_on_success() {
        let hook = ErrorTrackerHook::new(2);
        let mut ctx = HookContext::new("s1", "agent");
        ctx.tool_name = Some("bash".to_string());

        // 1 error
        ctx.error = Some("fail".to_string());
        let _ = hook.execute(&mut ctx).await;

        // success resets
        ctx.error = None;
        let _ = hook.execute(&mut ctx).await;

        // 1 error again — should not abort
        ctx.error = Some("fail".to_string());
        let r = hook.execute(&mut ctx).await;
        assert!(matches!(r, HookResult::Continue));
    }

    #[tokio::test]
    async fn audit_trail_always_continues() {
        for hook in AuditTrailHook::all() {
            let mut ctx = HookContext::new("s1", "agent");
            let result = hook.execute(&mut ctx).await;
            assert!(matches!(result, HookResult::Continue));
        }
    }
}
