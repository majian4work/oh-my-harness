use regex::Regex;
use serde::{Deserialize, Serialize};
use tool::PermissionLevel;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
}

/// A permission rule matching tool name + optional input pattern.
/// Format: "tool_name" or "tool_name(pattern:*)"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool_pattern: String,
    pub input_pattern: Option<String>,
    pub action: RuleAction,
}

impl PermissionRule {
    pub fn allow(pattern: impl Into<String>) -> Self {
        Self {
            tool_pattern: pattern.into(),
            input_pattern: None,
            action: RuleAction::Allow,
        }
    }
    pub fn deny(pattern: impl Into<String>) -> Self {
        Self {
            tool_pattern: pattern.into(),
            input_pattern: None,
            action: RuleAction::Deny,
        }
    }
    pub fn ask(pattern: impl Into<String>) -> Self {
        Self {
            tool_pattern: pattern.into(),
            input_pattern: None,
            action: RuleAction::Ask,
        }
    }

    pub fn matches(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        // Parse "tool_name(input_pattern)" format
        if let Some(paren_start) = self.tool_pattern.find('(') {
            let name_part = &self.tool_pattern[..paren_start];
            if !glob_match(name_part, tool_name) {
                return false;
            }
            let inner = self.tool_pattern[paren_start + 1..].trim_end_matches(')');
            // Match inner against input's string representation
            let input_str = input.to_string();
            glob_match(inner, &input_str)
        } else {
            glob_match(&self.tool_pattern, tool_name)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub default_level: PermissionLevel,
    pub deny_rules: Vec<PermissionRule>,
    pub ask_rules: Vec<PermissionRule>,
    pub allow_rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny(String),
    Ask(String),
}

impl PermissionPolicy {
    pub fn permissive() -> Self {
        Self {
            default_level: PermissionLevel::FullAccess,
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            allow_rules: Vec::new(),
        }
    }

    pub fn read_only() -> Self {
        Self {
            default_level: PermissionLevel::ReadOnly,
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            allow_rules: Vec::new(),
        }
    }

    /// Evaluate permission for a tool call.
    /// Order: deny rules → ask rules → allow rules → level comparison.
    pub fn evaluate(
        &self,
        tool_name: &str,
        required: &PermissionLevel,
        input: &serde_json::Value,
    ) -> PermissionDecision {
        // 1. Check deny rules first
        for rule in &self.deny_rules {
            if rule.matches(tool_name, input) {
                return PermissionDecision::Deny(format!("denied by rule: {}", rule.tool_pattern));
            }
        }
        // 2. Check ask rules
        for rule in &self.ask_rules {
            if rule.matches(tool_name, input) {
                return PermissionDecision::Ask(format!(
                    "approval required: {}",
                    rule.tool_pattern
                ));
            }
        }
        // 3. Check allow rules
        for rule in &self.allow_rules {
            if rule.matches(tool_name, input) {
                return PermissionDecision::Allow;
            }
        }
        // 4. Compare permission levels
        if required <= &self.default_level {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Ask(format!(
                "tool '{tool_name}' requires {required:?}, policy allows {:?}",
                self.default_level
            ))
        }
    }
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::permissive()
    }
}

/// Simple glob matching: supports * as wildcard.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    let regex_pattern = format!("^{}$", regex::escape(pattern).replace(r"\*", ".*"));
    Regex::new(&regex_pattern)
        .map(|r| r.is_match(text))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_allows_everything() {
        let policy = PermissionPolicy::permissive();
        let d = policy.evaluate("bash", &PermissionLevel::FullAccess, &serde_json::json!({}));
        assert_eq!(d, PermissionDecision::Allow);
    }

    #[test]
    fn read_only_denies_write() {
        let policy = PermissionPolicy::read_only();
        let d = policy.evaluate(
            "write_file",
            &PermissionLevel::WorkspaceWrite,
            &serde_json::json!({}),
        );
        assert!(matches!(d, PermissionDecision::Ask(_)));
    }

    #[test]
    fn deny_rule_overrides() {
        let policy = PermissionPolicy {
            default_level: PermissionLevel::FullAccess,
            deny_rules: vec![PermissionRule::deny("bash")],
            ask_rules: Vec::new(),
            allow_rules: Vec::new(),
        };
        let d = policy.evaluate("bash", &PermissionLevel::FullAccess, &serde_json::json!({}));
        assert!(matches!(d, PermissionDecision::Deny(_)));
    }

    #[test]
    fn allow_rule_overrides_level() {
        let policy = PermissionPolicy {
            default_level: PermissionLevel::ReadOnly,
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            allow_rules: vec![PermissionRule::allow("bash")],
        };
        let d = policy.evaluate("bash", &PermissionLevel::FullAccess, &serde_json::json!({}));
        assert_eq!(d, PermissionDecision::Allow);
    }

    #[test]
    fn wildcard_pattern_matching() {
        let rule = PermissionRule::allow("read_*");
        assert!(rule.matches("read_file", &serde_json::json!({})));
        assert!(!rule.matches("write_file", &serde_json::json!({})));
    }
}
