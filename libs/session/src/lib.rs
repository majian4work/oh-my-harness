use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use message::{Message, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub title: String,
    pub agent_name: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub workspace_root: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub title: String,
    pub agent_name: String,
    pub model: String,
    pub message_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Header stored as the first line of each JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub title: String,
    pub agent_name: String,
    pub model: String,
    pub workspace_root: PathBuf,
    pub created_at: i64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

pub struct SessionManager {
    sessions_dir: PathBuf,
}

impl SessionManager {
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = sessions_dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { sessions_dir: dir })
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        if let Some((parent_id, agent_name)) = id.split_once(':') {
            self.sessions_dir
                .join(parent_id)
                .join("agents")
                .join(agent_name)
        } else {
            self.sessions_dir.join(id)
        }
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("session.jsonl")
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("trace.log")
    }

    pub fn dump_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("dumps")
    }

    pub fn telemetry_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("telemetry.jsonl")
    }

    pub fn tool_telemetry_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("tool_telemetry.jsonl")
    }

    pub fn create(&self, agent: &str, model: &str, workspace_root: &Path) -> Result<Session> {
        let id = format!("ses_{}", uuid::Uuid::new_v4().simple());
        let now = now_millis();
        let session = Session {
            id: id.clone(),
            parent_id: None,
            title: String::new(),
            agent_name: agent.into(),
            model: model.into(),
            messages: Vec::new(),
            workspace_root: workspace_root.into(),
            created_at: now,
            updated_at: now,
            input_tokens: 0,
            output_tokens: 0,
        };
        let header = SessionHeader {
            id: session.id.clone(),
            parent_id: None,
            title: session.title.clone(),
            agent_name: session.agent_name.clone(),
            model: session.model.clone(),
            workspace_root: session.workspace_root.clone(),
            created_at: now,
            input_tokens: 0,
            output_tokens: 0,
        };
        fs::create_dir_all(self.session_dir(&session.id))?;
        let path = self.session_path(&session.id);
        let mut f = fs::File::create(&path)?;
        writeln!(f, "{}", serde_json::to_string(&header)?)?;
        tracing::info!(session_id = %session.id, agent = agent, "session created");
        Ok(session)
    }

    pub fn create_child(
        &self,
        parent_id: &str,
        agent: &str,
        model: &str,
        workspace_root: &Path,
    ) -> Result<Session> {
        let mut session = self.create(agent, model, workspace_root)?;
        session.parent_id = Some(parent_id.into());
        let header = SessionHeader {
            id: session.id.clone(),
            parent_id: session.parent_id.clone(),
            title: session.title.clone(),
            agent_name: session.agent_name.clone(),
            model: session.model.clone(),
            workspace_root: session.workspace_root.clone(),
            created_at: session.created_at,
            input_tokens: session.input_tokens,
            output_tokens: session.output_tokens,
        };
        let path = self.session_path(&session.id);
        let mut f = fs::File::create(&path)?;
        writeln!(f, "{}", serde_json::to_string(&header)?)?;
        Ok(session)
    }

    pub fn create_subagent_session(
        &self,
        parent_id: &str,
        agent_name: &str,
        model: &str,
        workspace_root: &Path,
    ) -> Result<Session> {
        let sub_id = format!("{parent_id}:{agent_name}");
        let sub_dir = self.session_dir(parent_id).join("agents").join(agent_name);
        fs::create_dir_all(&sub_dir)?;

        let now = now_millis();
        let session = Session {
            id: sub_id.clone(),
            parent_id: Some(parent_id.into()),
            title: String::new(),
            agent_name: agent_name.into(),
            model: model.into(),
            messages: Vec::new(),
            workspace_root: workspace_root.into(),
            created_at: now,
            updated_at: now,
            input_tokens: 0,
            output_tokens: 0,
        };
        let header = SessionHeader {
            id: session.id.clone(),
            parent_id: session.parent_id.clone(),
            title: session.title.clone(),
            agent_name: session.agent_name.clone(),
            model: session.model.clone(),
            workspace_root: session.workspace_root.clone(),
            created_at: now,
            input_tokens: 0,
            output_tokens: 0,
        };
        let path = sub_dir.join("session.jsonl");
        let mut f = fs::File::create(&path)?;
        writeln!(f, "{}", serde_json::to_string(&header)?)?;
        tracing::info!(session_id = %sub_id, parent = %parent_id, agent = agent_name, "subagent session created");
        Ok(session)
    }

    pub fn append_message(&self, session_id: &str, msg: &Message) -> Result<()> {
        let path = self.session_path(session_id);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("session not found: {session_id}"))?;
        writeln!(f, "{}", serde_json::to_string(msg)?)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Session> {
        let path = self.session_path(id);
        let file = fs::File::open(&path).with_context(|| format!("session not found: {id}"))?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();

        let header_line = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty session file"))??;
        let header: SessionHeader = serde_json::from_str(&header_line)?;

        let mut messages = Vec::new();
        let mut updated_at = header.created_at;
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(&line)?;
            updated_at = msg.created_at;
            messages.push(msg);
        }

        Ok(Session {
            id: header.id,
            parent_id: header.parent_id,
            title: header.title,
            agent_name: header.agent_name,
            model: header.model,
            messages,
            workspace_root: header.workspace_root,
            created_at: header.created_at,
            updated_at,
            input_tokens: header.input_tokens,
            output_tokens: header.output_tokens,
        })
    }

    pub fn resume(&self, id: &str) -> Result<Session> {
        self.get(id)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();

        entries.sort_by(|a, b| {
            let ma = a.metadata().and_then(|m| m.modified()).ok();
            let mb = b.metadata().and_then(|m| m.modified()).ok();
            mb.cmp(&ma)
        });

        let mut summaries = Vec::new();
        for entry in entries.into_iter().take(limit) {
            let session_file = entry.path().join("session.jsonl");
            if let Ok(file) = fs::File::open(&session_file) {
                let mut reader = BufReader::new(file);
                let mut header_line = String::new();
                if reader.read_line(&mut header_line).is_ok() {
                    if let Ok(header) = serde_json::from_str::<SessionHeader>(&header_line) {
                        let message_count = reader.lines().filter(|l| l.is_ok()).count();
                        let updated_at = entry
                            .metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(header.created_at);
                        summaries.push(SessionSummary {
                            id: header.id,
                            parent_id: header.parent_id,
                            title: header.title,
                            agent_name: header.agent_name,
                            model: header.model,
                            message_count,
                            created_at: header.created_at,
                            updated_at,
                        });
                    }
                }
            }
        }
        Ok(summaries)
    }

    pub fn update_title(&self, id: &str, title: &str) -> Result<()> {
        let path = self.session_path(id);
        let content = fs::read_to_string(&path)?;
        let mut lines: Vec<String> = content.lines().map(|line| line.to_owned()).collect();
        if lines.is_empty() {
            bail!("empty session file");
        }
        let mut header: SessionHeader = serde_json::from_str(&lines[0])?;
        header.title = title.into();
        let new_header = serde_json::to_string(&header)?;
        lines[0] = new_header;
        fs::write(&path, lines.join("\n") + "\n")?;
        Ok(())
    }

    pub fn update_tokens(&self, id: &str, input_tokens: u64, output_tokens: u64) -> Result<()> {
        let path = self.session_path(id);
        let content = fs::read_to_string(&path)?;
        let mut lines: Vec<String> = content.lines().map(|line| line.to_owned()).collect();
        if lines.is_empty() {
            bail!("empty session file");
        }
        let mut header: SessionHeader = serde_json::from_str(&lines[0])?;
        header.input_tokens = input_tokens;
        header.output_tokens = output_tokens;
        lines[0] = serde_json::to_string(&header)?;
        fs::write(&path, lines.join("\n") + "\n")?;
        Ok(())
    }

    pub fn restore(&self, session: Session) -> Result<()> {
        let header = SessionHeader {
            id: session.id.clone(),
            parent_id: session.parent_id,
            title: session.title,
            agent_name: session.agent_name,
            model: session.model,
            workspace_root: session.workspace_root,
            created_at: session.created_at,
            input_tokens: session.input_tokens,
            output_tokens: session.output_tokens,
        };
        fs::create_dir_all(self.session_dir(&session.id))?;
        let path = self.session_path(&session.id);
        let mut f = fs::File::create(&path)?;
        writeln!(f, "{}", serde_json::to_string(&header)?)?;
        for msg in &session.messages {
            writeln!(f, "{}", serde_json::to_string(msg)?)?;
        }
        Ok(())
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use message::Message;
    use std::path::PathBuf;

    #[test]
    fn create_and_get_session() {
        let dir = tempdir();
        let mgr = SessionManager::new(&dir).unwrap();
        let session = mgr.create("build", "gpt-4o", Path::new("/tmp")).unwrap();
        assert!(session.id.starts_with("ses_"));
        assert_eq!(session.agent_name, "build");
        assert!(session.messages.is_empty());

        let loaded = mgr.get(&session.id).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.agent_name, "build");
        assert_eq!(loaded.input_tokens, 0);
        assert_eq!(loaded.output_tokens, 0);
    }

    #[test]
    fn append_and_retrieve_messages() {
        let dir = tempdir();
        let mgr = SessionManager::new(&dir).unwrap();
        let session = mgr.create("build", "gpt-4o", Path::new("/tmp")).unwrap();

        let msg = Message::user("m1", "hello");
        mgr.append_message(&session.id, &msg).unwrap();

        let loaded = mgr.get(&session.id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text(), "hello");
    }

    #[test]
    fn list_sessions() {
        let dir = tempdir();
        let mgr = SessionManager::new(&dir).unwrap();
        mgr.create("a", "m1", Path::new("/tmp")).unwrap();
        mgr.create("b", "m2", Path::new("/tmp")).unwrap();

        let list = mgr.list(10).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn update_and_restore_tokens() {
        let dir = tempdir();
        let mgr = SessionManager::new(&dir).unwrap();
        let session = mgr.create("build", "gpt-4o", Path::new("/tmp")).unwrap();

        mgr.update_tokens(&session.id, 123, 456).unwrap();

        let loaded = mgr.get(&session.id).unwrap();
        assert_eq!(loaded.input_tokens, 123);
        assert_eq!(loaded.output_tokens, 456);
    }

    #[test]
    fn child_session_has_parent() {
        let dir = tempdir();
        let mgr = SessionManager::new(&dir).unwrap();
        let parent = mgr.create("build", "gpt-4o", Path::new("/tmp")).unwrap();
        let child = mgr
            .create_child(&parent.id, "explore", "gpt-4o-mini", Path::new("/tmp"))
            .unwrap();

        let loaded = mgr.get(&child.id).unwrap();
        assert_eq!(loaded.parent_id.as_deref(), Some(parent.id.as_str()));
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omh_test_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
