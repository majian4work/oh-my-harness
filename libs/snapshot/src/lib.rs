use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalEntry {
    TurnStarted {
        turn_id: String,
        input: String,
        timestamp: i64,
    },
    LlmRequestSent {
        turn_id: String,
        request_hash: u64,
    },
    LlmResponseChunk {
        turn_id: String,
        chunk_index: u32,
        text: String,
    },
    ToolCallStarted {
        turn_id: String,
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolCallCompleted {
        turn_id: String,
        call_id: String,
        output: String,
        is_error: bool,
    },
    ToolCallFailed {
        turn_id: String,
        call_id: String,
        error: String,
    },
    TurnCompleted {
        turn_id: String,
        timestamp: i64,
    },
    Checkpoint {
        session_bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct Wal {
    path: PathBuf,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create WAL parent directory {}", parent.display())
            })?;
        }

        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open WAL at {}", path.display()))?;

        Ok(Self { path })
    }

    pub fn append(&self, entry: &WalEntry) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open WAL for append at {}", self.path.display()))?;

        serde_json::to_writer(&mut file, entry).with_context(|| {
            format!("failed to serialize WAL entry for {}", self.path.display())
        })?;
        file.write_all(b"\n")
            .with_context(|| format!("failed to write WAL newline to {}", self.path.display()))?;
        file.sync_data()
            .with_context(|| format!("failed to fsync WAL at {}", self.path.display()))?;

        Ok(())
    }

    pub fn replay(&self) -> Result<Vec<WalEntry>> {
        Self::read_entries(&self.path)
    }

    pub fn truncate_after_checkpoint(&self) -> Result<()> {
        let entries = self.replay()?;
        let Some(last_checkpoint_index) = entries
            .iter()
            .rposition(|entry| matches!(entry, WalEntry::Checkpoint { .. }))
        else {
            return Ok(());
        };

        self.rewrite_entries(&entries[..=last_checkpoint_index])
    }

    pub fn last_consistent_state(&self) -> Result<Vec<WalEntry>> {
        let entries = self.replay()?;
        let Some(last_consistent_index) = entries.iter().rposition(|entry| {
            matches!(
                entry,
                WalEntry::TurnCompleted { .. } | WalEntry::ToolCallCompleted { .. }
            )
        }) else {
            return Ok(Vec::new());
        };

        Ok(entries
            .into_iter()
            .take(last_consistent_index + 1)
            .collect())
    }

    pub fn is_incomplete(&self) -> Result<bool> {
        let mut open_turns = HashSet::new();

        for entry in self.replay()? {
            match entry {
                WalEntry::TurnStarted { turn_id, .. } => {
                    open_turns.insert(turn_id);
                }
                WalEntry::TurnCompleted { turn_id, .. } => {
                    open_turns.remove(&turn_id);
                }
                _ => {}
            }
        }

        Ok(!open_turns.is_empty())
    }

    fn read_entries(path: &Path) -> Result<Vec<WalEntry>> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("failed to open WAL for replay at {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for (index, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "failed to read WAL line {} from {}",
                    index + 1,
                    path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }

            let entry = serde_json::from_str::<WalEntry>(&line).with_context(|| {
                format!(
                    "failed to deserialize WAL line {} from {}",
                    index + 1,
                    path.display()
                )
            })?;
            entries.push(entry);
        }

        Ok(entries)
    }

    fn rewrite_entries(&self, entries: &[WalEntry]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .with_context(|| format!("failed to rewrite WAL at {}", self.path.display()))?;

        for entry in entries {
            serde_json::to_writer(&mut file, entry).with_context(|| {
                format!("failed to serialize WAL entry for {}", self.path.display())
            })?;
            file.write_all(b"\n").with_context(|| {
                format!("failed to write WAL newline to {}", self.path.display())
            })?;
        }

        file.sync_data()
            .with_context(|| format!("failed to fsync rewritten WAL at {}", self.path.display()))?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GitSnapshot {
    workspace_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub files_changed: Vec<String>,
    pub additions: usize,
    pub deletions: usize,
}

impl GitSnapshot {
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
        }
    }

    pub fn before_turn(&self, session_id: &str) -> Result<SnapshotId> {
        let snapshot = self.git([
            "stash",
            "create",
            &format!("snapshot-before-turn-{session_id}"),
        ])?;
        let snapshot = snapshot.trim();

        if !snapshot.is_empty() {
            return Ok(SnapshotId(snapshot.to_owned()));
        }

        let head = self.git(["rev-parse", "HEAD"])?;
        Ok(SnapshotId(head.trim().to_owned()))
    }

    pub fn after_turn(&self, snapshot_id: &SnapshotId) -> Result<FileDiff> {
        let names = self.git(["diff", "--name-only", &snapshot_id.0, "--"])?;
        let files_changed = names
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let numstat = self.git(["diff", "--numstat", &snapshot_id.0, "--"])?;
        let mut additions = 0usize;
        let mut deletions = 0usize;

        for line in numstat
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let mut parts = line.split('\t');
            let added = parts
                .next()
                .context("missing additions column from git diff --numstat")?;
            let removed = parts
                .next()
                .context("missing deletions column from git diff --numstat")?;

            if added != "-" {
                additions += added
                    .parse::<usize>()
                    .with_context(|| format!("invalid additions count from git diff: {added}"))?;
            }

            if removed != "-" {
                deletions += removed
                    .parse::<usize>()
                    .with_context(|| format!("invalid deletions count from git diff: {removed}"))?;
            }
        }

        Ok(FileDiff {
            files_changed,
            additions,
            deletions,
        })
    }

    pub fn revert_to(&self, snapshot_id: &SnapshotId) -> Result<()> {
        self.git(["checkout", &snapshot_id.0])?;
        Ok(())
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String> {
        tracing::debug!(workspace = %self.workspace_root.display(), ?args, "running git command");

        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workspace_root)
            .output()
            .with_context(|| {
                format!(
                    "failed to spawn git in {} with args {:?}",
                    self.workspace_root.display(),
                    args
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git {:?} failed in {}: {}",
                args,
                self.workspace_root.display(),
                stderr.trim()
            );
        }

        String::from_utf8(output.stdout)
            .with_context(|| format!("git {:?} returned non-utf8 stdout", args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsStr;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("snapshot-{}", ulid::Ulid::new()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn wal_append_and_replay_round_trip() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.path().join("wal.jsonl")).expect("open wal");
        let entries = vec![
            WalEntry::TurnStarted {
                turn_id: "turn-1".to_owned(),
                input: "hello".to_owned(),
                timestamp: 1,
            },
            WalEntry::ToolCallStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-1".to_owned(),
                tool: "search".to_owned(),
                input: json!({"q": "rust"}),
            },
            WalEntry::ToolCallCompleted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-1".to_owned(),
                output: "ok".to_owned(),
                is_error: false,
            },
            WalEntry::Checkpoint {
                session_bytes: vec![1, 2, 3],
            },
        ];

        for entry in &entries {
            wal.append(entry).expect("append wal entry");
        }

        assert_eq!(wal.replay().expect("replay wal"), entries);
    }

    #[test]
    fn truncate_after_checkpoint_keeps_entries_through_last_checkpoint() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.path().join("wal.jsonl")).expect("open wal");
        let entries = vec![
            WalEntry::TurnStarted {
                turn_id: "turn-1".to_owned(),
                input: "hello".to_owned(),
                timestamp: 1,
            },
            WalEntry::Checkpoint {
                session_bytes: vec![1],
            },
            WalEntry::ToolCallStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-1".to_owned(),
                tool: "search".to_owned(),
                input: json!({"q": "ignored"}),
            },
            WalEntry::Checkpoint {
                session_bytes: vec![2],
            },
            WalEntry::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                timestamp: 2,
            },
        ];

        for entry in &entries {
            wal.append(entry).expect("append wal entry");
        }

        wal.truncate_after_checkpoint()
            .expect("truncate after checkpoint");

        assert_eq!(
            wal.replay().expect("replay wal"),
            vec![
                WalEntry::TurnStarted {
                    turn_id: "turn-1".to_owned(),
                    input: "hello".to_owned(),
                    timestamp: 1,
                },
                WalEntry::Checkpoint {
                    session_bytes: vec![1],
                },
                WalEntry::ToolCallStarted {
                    turn_id: "turn-1".to_owned(),
                    call_id: "call-1".to_owned(),
                    tool: "search".to_owned(),
                    input: json!({"q": "ignored"}),
                },
                WalEntry::Checkpoint {
                    session_bytes: vec![2],
                },
            ]
        );
    }

    #[test]
    fn last_consistent_state_uses_last_completed_boundary() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.path().join("wal.jsonl")).expect("open wal");
        let expected = vec![
            WalEntry::TurnStarted {
                turn_id: "turn-1".to_owned(),
                input: "hello".to_owned(),
                timestamp: 1,
            },
            WalEntry::ToolCallStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-1".to_owned(),
                tool: "search".to_owned(),
                input: json!({"q": "rust"}),
            },
            WalEntry::ToolCallCompleted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-1".to_owned(),
                output: "done".to_owned(),
                is_error: false,
            },
        ];

        for entry in expected.iter().cloned().chain([
            WalEntry::LlmResponseChunk {
                turn_id: "turn-1".to_owned(),
                chunk_index: 0,
                text: "partial".to_owned(),
            },
            WalEntry::ToolCallStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "call-2".to_owned(),
                tool: "edit".to_owned(),
                input: json!({"file": "src/lib.rs"}),
            },
        ]) {
            wal.append(&entry).expect("append wal entry");
        }

        assert_eq!(
            wal.last_consistent_state().expect("last consistent state"),
            expected
        );

        let second_wal = Wal::open(dir.path().join("wal-2.jsonl")).expect("open second wal");
        let second_expected = vec![
            WalEntry::TurnStarted {
                turn_id: "turn-a".to_owned(),
                input: "hello".to_owned(),
                timestamp: 10,
            },
            WalEntry::TurnCompleted {
                turn_id: "turn-a".to_owned(),
                timestamp: 11,
            },
        ];

        for entry in second_expected
            .iter()
            .cloned()
            .chain([WalEntry::TurnStarted {
                turn_id: "turn-b".to_owned(),
                input: "still running".to_owned(),
                timestamp: 12,
            }])
        {
            second_wal.append(&entry).expect("append second wal entry");
        }

        assert_eq!(
            second_wal
                .last_consistent_state()
                .expect("second last consistent state"),
            second_expected
        );
    }

    #[test]
    fn is_incomplete_detects_unfinished_turns() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.path().join("wal.jsonl")).expect("open wal");

        wal.append(&WalEntry::TurnStarted {
            turn_id: "turn-1".to_owned(),
            input: "hello".to_owned(),
            timestamp: 1,
        })
        .expect("append turn started");

        assert!(wal.is_incomplete().expect("detect incomplete state"));

        wal.append(&WalEntry::TurnCompleted {
            turn_id: "turn-1".to_owned(),
            timestamp: 2,
        })
        .expect("append turn completed");

        assert!(!wal.is_incomplete().expect("detect complete state"));
    }

    #[test]
    #[ignore = "requires git CLI and repository setup"]
    fn git_snapshot_reports_file_diff() {
        let dir = TestDir::new();
        run_git(dir.path(), ["init"]);
        fs::write(dir.path().join("tracked.txt"), "before\n").expect("write tracked file");
        run_git(dir.path(), ["add", "tracked.txt"]);
        run_git_with_identity(dir.path(), ["commit", "-m", "initial"]);

        let snapshot = GitSnapshot::new(dir.path());
        let before = snapshot.before_turn("session-1").expect("create snapshot");

        fs::write(dir.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked file");

        let diff = snapshot.after_turn(&before).expect("compute diff");

        assert_eq!(diff.files_changed, vec!["tracked.txt".to_owned()]);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 0);
    }

    fn run_git<I, S>(dir: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");

        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_git_with_identity<I, S>(dir: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .args([
                OsStr::new("-c"),
                OsStr::new("user.name=Snapshot Test"),
                OsStr::new("-c"),
                OsStr::new("user.email=snapshot@example.com"),
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git with identity");

        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
