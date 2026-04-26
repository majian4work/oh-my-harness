//! Team manager — tracks omh instances that have joined the omt team.
//!
//! Supports:
//! - Multiple same-role agents → round-robin / least-loaded dispatch
//! - Different-role agents → role-based task routing

use std::collections::HashMap;
use std::sync::Arc;

use a2a::{
    MemberStatus, TeamHeartbeatRequest, TeamJoinRequest, TeamJoinResponse, TeamLeaveRequest,
    TeamMember, TeamStatusResponse,
};
use tokio::sync::RwLock;

/// Manages the set of team members (remote omh instances).
#[derive(Clone)]
pub struct TeamManager {
    inner: Arc<RwLock<TeamState>>,
}

struct TeamState {
    members: HashMap<String, TeamMember>,
    /// Round-robin counters per role.
    robin: HashMap<String, usize>,
}

impl TeamManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TeamState {
                members: HashMap::new(),
                robin: HashMap::new(),
            })),
        }
    }

    /// Handle a join request. Returns the response with an assigned instance id.
    pub async fn handle_join(&self, req: TeamJoinRequest) -> TeamJoinResponse {
        let instance_id = format!(
            "{}-{}",
            req.card.name,
            chrono::Utc::now().timestamp_millis()
        );
        let member = TeamMember {
            instance_id: instance_id.clone(),
            card: req.card,
            endpoint: req.endpoint,
            role: req.role,
            capacity: req.capacity,
            active_tasks: 0,
            status: MemberStatus::Active,
            joined_at: chrono::Utc::now().to_rfc3339(),
            last_heartbeat: Some(chrono::Utc::now().to_rfc3339()),
        };

        let mut state = self.inner.write().await;
        tracing::info!(
            "team join: {} role={} capacity={} endpoint={}",
            member.instance_id,
            member.role,
            member.capacity,
            member.endpoint,
        );
        state.members.insert(instance_id.clone(), member);

        TeamJoinResponse {
            accepted: true,
            instance_id: Some(instance_id),
            heartbeat_interval_secs: 30,
            message: None,
        }
    }

    /// Handle a leave request.
    pub async fn handle_leave(&self, req: TeamLeaveRequest) {
        let mut state = self.inner.write().await;
        if state.members.remove(&req.instance_id).is_some() {
            tracing::info!("team leave: {}", req.instance_id);
        }
    }

    /// Handle a heartbeat.
    pub async fn handle_heartbeat(&self, req: TeamHeartbeatRequest) {
        let mut state = self.inner.write().await;
        if let Some(member) = state.members.get_mut(&req.instance_id) {
            member.last_heartbeat = Some(chrono::Utc::now().to_rfc3339());
            member.active_tasks = req.active_tasks;
        }
    }

    /// Return current team status.
    pub async fn status(&self) -> TeamStatusResponse {
        let state = self.inner.read().await;
        TeamStatusResponse {
            members: state.members.values().cloned().collect(),
        }
    }

    /// Pick the best member for a given role. Uses least-loaded with
    /// round-robin tiebreaker. Returns `(instance_id, endpoint)`.
    pub async fn pick_member(&self, role: &str) -> Option<(String, String)> {
        let mut state = self.inner.write().await;

        // Collect owned candidate info to avoid borrow conflict
        let mut candidates: Vec<(String, String, f64)> = state
            .members
            .values()
            .filter(|m| m.role == role && m.status == MemberStatus::Active && m.active_tasks < m.capacity)
            .map(|m| {
                let load = m.active_tasks as f64 / m.capacity.max(1) as f64;
                (m.instance_id.clone(), m.endpoint.clone(), load)
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

        let counter = state.robin.entry(role.to_string()).or_insert(0);
        let idx = *counter % candidates.len();
        *counter = counter.wrapping_add(1);

        let (instance_id, endpoint, _) = &candidates[idx];
        let instance_id = instance_id.clone();
        let endpoint = endpoint.clone();

        // Increment active_tasks for the selected member
        if let Some(member) = state.members.get_mut(&instance_id) {
            member.active_tasks += 1;
        }

        Some((instance_id, endpoint))
    }

    /// Mark a task as completed on a member (decrement active_tasks).
    pub async fn task_done(&self, instance_id: &str) {
        let mut state = self.inner.write().await;
        if let Some(member) = state.members.get_mut(instance_id) {
            member.active_tasks = member.active_tasks.saturating_sub(1);
        }
    }

    /// List all distinct roles that have at least one active member.
    pub async fn available_roles(&self) -> Vec<String> {
        let state = self.inner.read().await;
        let mut roles: Vec<String> = state
            .members
            .values()
            .filter(|m| m.status == MemberStatus::Active)
            .map(|m| m.role.clone())
            .collect();
        roles.sort();
        roles.dedup();
        roles
    }

    /// Check if there are any active team members.
    pub async fn has_members(&self) -> bool {
        let state = self.inner.read().await;
        state
            .members
            .values()
            .any(|m| m.status == MemberStatus::Active)
    }

    /// Expire members whose last heartbeat is older than `timeout_secs`.
    pub async fn expire_stale(&self, timeout_secs: i64) {
        let mut state = self.inner.write().await;
        let now = chrono::Utc::now();
        let stale: Vec<String> = state
            .members
            .iter()
            .filter_map(|(id, m)| {
                let hb = m.last_heartbeat.as_deref().and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                });
                match hb {
                    Some(dt) if (now - dt).num_seconds() > timeout_secs => Some(id.clone()),
                    None => Some(id.clone()),
                    _ => None,
                }
            })
            .collect();

        for id in &stale {
            tracing::warn!("expiring stale team member: {id}");
            state.members.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{AgentCapabilities, AgentCard, AgentSkill};

    fn make_card(name: &str) -> AgentCard {
        AgentCard {
            name: name.to_string(),
            description: None,
            url: String::new(),
            provider: None,
            version: "1.0".to_string(),
            capabilities: AgentCapabilities::default(),
            skills: vec![AgentSkill {
                id: "code".to_string(),
                name: "Code".to_string(),
                description: None,
                tags: vec!["coding".to_string()],
                examples: vec![],
            }],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        }
    }

    #[tokio::test]
    async fn join_and_pick_member() {
        let mgr = TeamManager::new();
        let resp = mgr
            .handle_join(TeamJoinRequest {
                card: make_card("omh-1"),
                endpoint: "http://localhost:3001".to_string(),
                role: "coder".to_string(),
                capacity: 2,
            })
            .await;
        assert!(resp.accepted);
        assert!(resp.instance_id.is_some());

        let pick = mgr.pick_member("coder").await;
        assert!(pick.is_some());
        let (iid, ep) = pick.unwrap();
        assert!(iid.starts_with("omh-1-"));
        assert_eq!(ep, "http://localhost:3001");

        // No reviewer role available
        assert!(mgr.pick_member("reviewer").await.is_none());
    }

    #[tokio::test]
    async fn load_balancing() {
        let mgr = TeamManager::new();

        // Add two coders with capacity 1 each
        let r1 = mgr
            .handle_join(TeamJoinRequest {
                card: make_card("omh-a"),
                endpoint: "http://localhost:3001".to_string(),
                role: "coder".to_string(),
                capacity: 1,
            })
            .await;
        let r2 = mgr
            .handle_join(TeamJoinRequest {
                card: make_card("omh-b"),
                endpoint: "http://localhost:3002".to_string(),
                role: "coder".to_string(),
                capacity: 1,
            })
            .await;

        // First pick gets one, marks it busy
        let (id1, _) = mgr.pick_member("coder").await.unwrap();
        // Second pick should get the other (since first is at capacity)
        let (id2, _) = mgr.pick_member("coder").await.unwrap();
        assert_ne!(id1, id2);

        // Both at capacity now — no more available
        assert!(mgr.pick_member("coder").await.is_none());

        // Mark one done — should be pickable again
        mgr.task_done(&id1).await;
        assert!(mgr.pick_member("coder").await.is_some());

        // Suppress unused warnings
        let _ = (r1, r2);
    }

    #[tokio::test]
    async fn leave_removes_member() {
        let mgr = TeamManager::new();
        let resp = mgr
            .handle_join(TeamJoinRequest {
                card: make_card("omh-x"),
                endpoint: "http://localhost:3003".to_string(),
                role: "coder".to_string(),
                capacity: 1,
            })
            .await;
        let iid = resp.instance_id.unwrap();

        assert!(mgr.has_members().await);
        mgr.handle_leave(TeamLeaveRequest {
            instance_id: iid,
        })
        .await;
        assert!(!mgr.has_members().await);
    }
}
