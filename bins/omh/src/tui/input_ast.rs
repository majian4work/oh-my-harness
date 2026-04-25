use std::{fmt, ops::Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInput {
    pub raw: String,
    pub segments: Vec<ParsedInputSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInputSegment {
    Text(String),
    ExplicitAgent(ExplicitAgentToken),
    FileReference(FileReferenceToken),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitAgentToken {
    pub raw: String,
    pub name: String,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReferenceToken {
    pub raw: String,
    pub path: String,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingIntent {
    pub parsed_input: ParsedInput,
    pub explicit_agent: Option<ExplicitAgentToken>,
    pub file_references: Vec<FileReferenceToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashInvocation {
    pub raw: String,
    pub command: String,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitIntent {
    Slash(SlashInvocation),
    Chat(RoutingIntent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    MultipleExplicitAgents { first: String, second: String },
    UnsupportedExplicitAgent { agent: String },
    MalformedExplicitAgent { raw: String },
}

impl fmt::Display for IntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MultipleExplicitAgents { first, second } => write!(
                f,
                "Only one explicit @agent is allowed per message (found {first} and {second})."
            ),
            Self::UnsupportedExplicitAgent { agent } => {
                write!(f, "agent '{agent}' cannot be invoked explicitly")
            }
            Self::MalformedExplicitAgent { raw } => {
                write!(f, "malformed explicit @agent token '{raw}'")
            }
        }
    }
}

impl std::error::Error for IntentError {}
