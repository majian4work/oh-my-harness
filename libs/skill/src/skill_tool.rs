use std::sync::Arc;

use ::tool::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::SkillRegistry;

pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl ToolHandler for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".to_string(),
            description: "Load a skill by name to inject specialized instructions into context. Use when a task matches an available skill's domain."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            required_permission: PermissionLevel::ReadOnly,
            supports_parallel: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing 'name' parameter"))?;

        let Some(skill) = self.registry.get(name) else {
            let available = self
                .registry
                .on_demand()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(ToolOutput::error(format!(
                "Skill '{name}' not found. Available: {available}"
            )));
        };

        let mut content = format!(
            "<skill_content name=\"{}\">\n{}\n",
            skill.name, skill.content
        );
        if !skill.bundled_files.is_empty() {
            content.push_str("<bundled_files>\n");
            for file in &skill.bundled_files {
                content.push_str(&format!("- {}\n", file.display()));
            }
            content.push_str("</bundled_files>\n");
        }
        content.push_str("</skill_content>");

        Ok(ToolOutput::text(content))
    }
}
