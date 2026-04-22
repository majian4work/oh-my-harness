use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub const TOOL_CALL: &str = "tool_call";
pub const LLM_REQUEST: &str = "llm_request";
pub const LLM_RESPONSE: &str = "llm_response";
pub const AGENT_TURN: &str = "agent_turn";
pub const SUBAGENT_SPAWN: &str = "subagent_spawn";
pub const MEMORY_OP: &str = "memory_op";
pub const SNAPSHOT_OP: &str = "snapshot_op";

const TUI_LOG_CAPACITY: usize = 500;

const OMH_CRATES: &[&str] = &[
    "omh", "runtime", "provider", "tool", "agent", "session", "mcp", "bus", "memory", "evolution",
    "snapshot", "hook", "permission", "skill", "acp", "message", "omh_trace",
];

/// Build a filter that sets external crates to `warn` and our crates to the
/// requested level.  This prevents hyper/reqwest/tokio/h2/rustls trace spam
/// from flooding the log buffer and blocking the TUI.
fn build_scoped_filter(level: Level) -> EnvFilter {
    let mut directives = String::from("warn");
    for crate_name in OMH_CRATES {
        directives.push_str(&format!(",{}={}", crate_name, level.as_str()));
    }
    EnvFilter::new(directives)
}

pub fn init(level: Level) {
    let filter =
        EnvFilter::try_from_env("OMH_LOG").unwrap_or_else(|_| build_scoped_filter(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}

#[derive(Clone)]
pub struct TuiLogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl TuiLogBuffer {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(TUI_LOG_CAPACITY))),
        }
    }

    fn push(&self, line: String) {
        let mut buf = self.inner.lock().unwrap();
        if buf.len() >= TUI_LOG_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    pub fn drain(&self) -> Vec<String> {
        let buf = self.inner.lock().unwrap();
        buf.iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

struct TuiLogLayer {
    buffer: TuiLogBuffer,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TuiLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let line = format!("{} {} {}", meta.level(), meta.target(), visitor.message,);
        self.buffer.push(line);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if !self.message.is_empty() {
            self.message
                .push_str(&format!(" {}={:?}", field.name(), value));
        } else {
            self.message = format!("{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if !self.message.is_empty() {
            self.message
                .push_str(&format!(" {}={}", field.name(), value));
        } else {
            self.message = format!("{}={}", field.name(), value);
        }
    }
}

pub fn init_tui(level: Level) -> TuiLogBuffer {
    let filter =
        EnvFilter::try_from_env("OMH_LOG").unwrap_or_else(|_| build_scoped_filter(level));

    let buffer = TuiLogBuffer::new();
    let layer = TuiLogLayer {
        buffer: buffer.clone(),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .init();

    buffer
}

#[cfg(test)]
pub fn init_test() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::info;

    #[test]
    fn init_test_logs_message() {
        init_test();
        info!("trace test message");
    }

    #[test]
    fn tui_log_buffer_captures() {
        let buf = TuiLogBuffer::new();
        buf.push("test line".to_string());
        assert_eq!(buf.len(), 1);
        let lines = buf.drain();
        assert_eq!(lines[0], "test line");
    }

    #[test]
    fn tui_log_buffer_ring() {
        let buf = TuiLogBuffer::new();
        for i in 0..600 {
            buf.push(format!("line {i}"));
        }
        assert_eq!(buf.len(), TUI_LOG_CAPACITY);
        let lines = buf.drain();
        assert_eq!(lines[0], "line 100");
    }
}
