use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub const TOOL_CALL: &str = "tool_call";
pub const LLM_REQUEST: &str = "llm_request";
pub const LLM_RESPONSE: &str = "llm_response";
pub const AGENT_TURN: &str = "agent_turn";
pub const SUBAGENT_SPAWN: &str = "subagent_spawn";
pub const MEMORY_OP: &str = "memory_op";

const OMH_CRATES: &[&str] = &[
    "omh",
    "runtime",
    "provider",
    "tool",
    "agent",
    "session",
    "mcp",
    "bus",
    "memory",
    "evolution",
    "hook",
    "permission",
    "skill",
    "acp",
    "message",
    "omh_trace",
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

/// Build a filter that only passes events at exactly `level` for our crates.
fn build_exact_level_filter(level: Level) -> tracing_subscriber::filter::Targets {
    let mut targets = tracing_subscriber::filter::Targets::new();
    for crate_name in OMH_CRATES {
        targets = targets.with_target(*crate_name, level);
    }
    targets
}

fn log_dir() -> std::path::PathBuf {
    dirs::log_dir()
}

fn make_file_layer(level: Level) -> Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync> {
    let appender = tracing_appender::rolling::daily(
        log_dir(),
        format!("{}.log", level.as_str().to_lowercase()),
    );
    Box::new(
        tracing_subscriber::fmt::layer()
            .with_writer(appender)
            .with_ansi(false)
            .with_target(true)
            .with_file(false)
            .with_line_number(false)
            .with_filter(build_exact_level_filter(level)),
    )
}

pub fn init(level: Level) {
    let filter = EnvFilter::try_from_env("OMH_LOG").unwrap_or_else(|_| build_scoped_filter(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}

/// Initialise file-based logging for the TUI (no stdout output).
/// Each level writes to its own file under `~/.cache/omh/logs/`.
/// Only levels enabled by `--log` (or `OMH_LOG`) are written.
pub fn init_file(level: Level) {
    let layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> = [
        Level::ERROR,
        Level::WARN,
        Level::INFO,
        Level::DEBUG,
        Level::TRACE,
    ]
    .into_iter()
    .filter(|l| *l <= level)
    .map(make_file_layer)
    .collect();

    tracing_subscriber::registry().with(layers).init();
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
}
