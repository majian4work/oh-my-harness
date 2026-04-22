pub mod bash;
pub mod file_ops;
pub mod search;

use crate::ToolRegistry;

pub fn register_builtins(registry: &ToolRegistry) {
    registry.register(Box::new(bash::BashTool));
    registry.register(Box::new(file_ops::ReadFileTool));
    registry.register(Box::new(file_ops::WriteFileTool));
    registry.register(Box::new(file_ops::EditFileTool));
    registry.register(Box::new(search::GlobTool));
    registry.register(Box::new(search::GrepTool));
}
