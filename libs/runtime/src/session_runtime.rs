use std::sync::Arc;

use anyhow::Result;
use provider::{Effort, ModelSpec};
use session::SessionState;

use crate::agent_runtime::{AgentRuntime, TurnResult};
use crate::harness::Harness;

const DEFAULT_AGENT: &str = "orchestrator";

/// Long-lived runtime bound to a single session.
///
/// Manages persistent state (`SessionState`) and creates short-lived
/// `AgentRuntime` instances per user message.
pub struct SessionRuntime {
    pub session_id: String,
    pub harness: Arc<Harness>,
    state: SessionState,
}

impl SessionRuntime {
    /// Create a new `SessionRuntime`, loading persisted state from `state.json`.
    pub fn new(session_id: String, harness: Arc<Harness>) -> Self {
        let state = harness.session_manager.load_state(&session_id);
        Self {
            session_id,
            harness,
            state,
        }
    }

    // -- accessors --

    pub fn foreground_agent(&self) -> &str {
        self.state
            .foreground_agent
            .as_deref()
            .unwrap_or(DEFAULT_AGENT)
    }

    pub fn set_foreground_agent(&mut self, agent: &str) {
        self.state.foreground_agent = Some(agent.to_string());
        self.save_state();
    }

    pub fn turn_counter(&self) -> u32 {
        self.state.turn_counter
    }

    // -- turn execution --

    /// Run a foreground turn using the session's current agent.
    pub async fn run_turn(
        &mut self,
        input: &str,
        model_override: ModelSpec,
        effort_override: Option<Effort>,
    ) -> Result<TurnResult> {
        let agent_name = self.foreground_agent().to_string();
        let max_turns = self
            .harness
            .agent_registry
            .get(&agent_name)
            .and_then(|a| a.max_turns)
            .unwrap_or(30);

        let mut runtime = AgentRuntime::new(agent_name, self.session_id.clone(), max_turns);
        runtime = runtime.with_logger(&self.harness);
        runtime.model_override = Some(model_override);
        runtime.effort_override = effort_override;
        runtime.shared_harness = Some(self.harness.clone());
        runtime.current_turn = self.state.turn_counter;

        let result = runtime.run_turn(&self.harness, input).await;

        // Persist updated turn counter regardless of success/failure.
        self.state.turn_counter = runtime.current_turn;
        self.save_state();

        result
    }

    fn save_state(&self) {
        if let Err(e) = self
            .harness
            .session_manager
            .save_state(&self.session_id, &self.state)
        {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "failed to persist session state"
            );
        }
    }
}
