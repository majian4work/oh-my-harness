use std::collections::HashSet;

use super::input_ast::{
    ExplicitAgentToken, FileReferenceToken, IntentError, ParsedInput, ParsedInputSegment,
    RoutingIntent, SubmitIntent,
};
use super::mention_parser::{MentionCandidate, scan_mentions};
use super::slash_input::parse_slash_invocation;

pub struct InputIntentResolver {
    explicit_agents: HashSet<String>,
    known_agents: HashSet<String>,
}

impl InputIntentResolver {
    pub fn new<I, S, J, T>(explicit_agents: I, known_agents: J) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        J: IntoIterator<Item = T>,
        T: Into<String>,
    {
        Self {
            explicit_agents: explicit_agents.into_iter().map(Into::into).collect(),
            known_agents: known_agents.into_iter().map(Into::into).collect(),
        }
    }

    pub fn resolve(&self, input: &str) -> Result<SubmitIntent, IntentError> {
        if let Some(slash) = parse_slash_invocation(input) {
            return Ok(SubmitIntent::Slash(slash));
        }

        Ok(SubmitIntent::Chat(self.resolve_chat(input)?))
    }

    fn resolve_chat(&self, input: &str) -> Result<RoutingIntent, IntentError> {
        let mentions = scan_mentions(input);
        let mut segments = Vec::new();
        let mut explicit_agent: Option<ExplicitAgentToken> = None;
        let mut file_references = Vec::new();
        let mut cursor = 0;

        for mention in mentions {
            if mention.span.start > cursor {
                segments.push(ParsedInputSegment::Text(
                    input[cursor..mention.span.start].to_string(),
                ));
            }

            if mention.body.is_empty() {
                return Err(IntentError::MalformedExplicitAgent {
                    raw: mention.raw.clone(),
                });
            }

            if self.is_explicit_agent(&mention) {
                let token = ExplicitAgentToken {
                    raw: mention.raw.clone(),
                    name: mention.body.clone(),
                    span: mention.span.clone(),
                };

                if let Some(first) = explicit_agent.as_ref() {
                    return Err(IntentError::MultipleExplicitAgents {
                        first: first.raw.clone(),
                        second: token.raw.clone(),
                    });
                }

                explicit_agent = Some(token.clone());
                segments.push(ParsedInputSegment::ExplicitAgent(token));
            } else if self.is_known_agent(&mention) {
                return Err(IntentError::UnsupportedExplicitAgent {
                    agent: mention.body.clone(),
                });
            } else {
                let token = FileReferenceToken {
                    raw: mention.raw.clone(),
                    path: mention.body.clone(),
                    span: mention.span.clone(),
                };
                file_references.push(token.clone());
                segments.push(ParsedInputSegment::FileReference(token));
            }

            cursor = mention.span.end;
        }

        if cursor < input.len() || segments.is_empty() {
            segments.push(ParsedInputSegment::Text(input[cursor..].to_string()));
        }

        Ok(RoutingIntent {
            parsed_input: ParsedInput {
                raw: input.to_string(),
                segments,
            },
            explicit_agent,
            file_references,
        })
    }

    fn is_explicit_agent(&self, mention: &MentionCandidate) -> bool {
        self.explicit_agents.contains(mention.body.as_str())
    }

    fn is_known_agent(&self, mention: &MentionCandidate) -> bool {
        self.known_agents.contains(mention.body.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::InputIntentResolver;
    use crate::tui::input_ast::{ParsedInputSegment, SubmitIntent};

    #[test]
    fn parses_explicit_agent_and_file_context_into_routing_intent() {
        let resolver = InputIntentResolver::new(["explore", "oracle"], ["explore", "oracle"]);
        let input = "Please ask @explore to inspect @src/main.rs with @Cargo.toml";
        let intent = resolver.resolve(input).expect("intent should parse");

        let SubmitIntent::Chat(intent) = intent else {
            panic!("expected chat intent");
        };

        assert_eq!(
            intent
                .explicit_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("explore")
        );
        assert_eq!(
            intent
                .file_references
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/main.rs", "Cargo.toml"]
        );
        assert!(matches!(
            &intent.parsed_input.segments[1],
            ParsedInputSegment::ExplicitAgent(agent) if agent.name == "explore"
        ));
        assert!(matches!(
            &intent.parsed_input.segments[3],
            ParsedInputSegment::FileReference(file) if file.path == "src/main.rs"
        ));
        assert!(matches!(
            &intent.parsed_input.segments[5],
            ParsedInputSegment::FileReference(file) if file.path == "Cargo.toml"
        ));
    }

    #[test]
    fn rejects_multiple_explicit_agents() {
        let resolver = InputIntentResolver::new(["explore", "oracle"], ["explore", "oracle"]);
        let error = resolver
            .resolve("Ask @explore and @oracle to compare notes")
            .expect_err("multiple explicit agents should fail");

        assert_eq!(
            error.to_string(),
            "Only one explicit @agent is allowed per message (found @explore and @oracle)."
        );
    }

    #[test]
    fn keeps_slash_commands_out_of_chat_routing_resolution() {
        let resolver = InputIntentResolver::new(["explore"], ["explore"]);
        let intent = resolver.resolve("/models refresh").expect("slash intent");

        let SubmitIntent::Slash(intent) = intent else {
            panic!("expected slash intent");
        };
        assert_eq!(intent.command, "models");
        assert_eq!(intent.args, "refresh");
    }

    #[test]
    fn keeps_agent_switch_slash_commands_out_of_chat_routing_resolution() {
        let resolver = InputIntentResolver::new(["explore"], ["explore"]);
        let intent = resolver.resolve("/agent planner").expect("slash intent");

        let SubmitIntent::Slash(intent) = intent else {
            panic!("expected slash intent");
        };
        assert_eq!(intent.command, "agent");
        assert_eq!(intent.args, "planner");
    }

    #[test]
    fn rejects_non_invocable_explicit_agent_mentions() {
        let resolver = InputIntentResolver::new(["explore"], ["explore", "oracle"]);
        let error = resolver
            .resolve("Please ask @oracle to inspect this")
            .expect_err("non-invocable explicit agent should fail");

        assert_eq!(error.to_string(), "agent 'oracle' cannot be invoked explicitly");
    }

    #[test]
    fn rejects_builtin_non_invocable_explicit_agent_mentions() {
        let resolver = InputIntentResolver::new(["explore"], ["explore", "worker"]);
        let error = resolver
            .resolve("Please ask @worker to inspect this")
            .expect_err("non-invocable builtin explicit agent should fail");

        assert_eq!(error.to_string(), "agent 'worker' cannot be invoked explicitly");
    }

    #[test]
    fn rejects_malformed_explicit_agent_tokens() {
        let resolver = InputIntentResolver::new(["explore"], ["explore"]);
        let error = resolver
            .resolve("Please ask @ to inspect this")
            .expect_err("malformed explicit agent should fail");

        assert_eq!(error.to_string(), "malformed explicit @agent token '@'");
    }

    #[test]
    fn parses_file_context_before_explicit_agent_into_routing_intent() {
        let resolver = InputIntentResolver::new(["explore", "oracle"], ["explore", "oracle"]);
        let input = "Please inspect @Cargo.toml with @explore";
        let intent = resolver.resolve(input).expect("intent should parse");

        let SubmitIntent::Chat(intent) = intent else {
            panic!("expected chat intent");
        };

        assert_eq!(
            intent
                .explicit_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("explore")
        );
        assert_eq!(
            intent
                .file_references
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["Cargo.toml"]
        );
        assert!(matches!(
            &intent.parsed_input.segments[1],
            ParsedInputSegment::FileReference(file) if file.path == "Cargo.toml"
        ));
        assert!(matches!(
            &intent.parsed_input.segments[3],
            ParsedInputSegment::ExplicitAgent(agent) if agent.name == "explore"
        ));
    }
}
