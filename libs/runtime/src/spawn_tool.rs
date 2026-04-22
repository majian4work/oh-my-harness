use async_trait::async_trait;
use serde_json::{Value, json};
use tool::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolSpec};

pub struct SpawnAgentTool;

#[async_trait]
impl ToolHandler for SpawnAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_agent".to_string(),
            description: "Delegate work to another agent. Use background=true for independent tasks; foreground for tasks you need results from immediately.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Agent to spawn"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Prompt to send to the spawned agent"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Whether to run the agent as a background task"
                    }
                },
                "required": ["agent_name", "prompt", "background"],
                "additionalProperties": false
            }),
            required_permission: PermissionLevel::WorkspaceWrite,
            supports_parallel: false,
        }
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::error(
            "spawn_agent must be executed through AgentRuntime, not directly",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_tool_has_expected_spec() {
        let spec = SpawnAgentTool.spec();

        assert_eq!(spec.name, "spawn_agent");
        assert_eq!(spec.required_permission, PermissionLevel::WorkspaceWrite);
        assert!(!spec.supports_parallel);
        assert_eq!(
            spec.input_schema["required"],
            json!(["agent_name", "prompt", "background"])
        );
    }
}
