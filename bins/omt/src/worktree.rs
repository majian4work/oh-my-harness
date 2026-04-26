use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Create a git worktree for a task under `.omt/<task_id>`.
/// Returns the absolute path of the worktree.
pub fn create_worktree(task_id: &str, branch_name: &str) -> Result<String> {
    let workspace_root = std::env::current_dir()?;
    let worktree_path = workspace_root.join(".omt").join(task_id);

    // Ensure .omt is in .gitignore
    ensure_gitignore(&workspace_root)?;

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            "-b",
            branch_name,
        ])
        .current_dir(&workspace_root)
        .output()
        .context("failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If branch already exists (resume scenario), try without -b
        if stderr.contains("already exists") {
            let output2 = Command::new("git")
                .args([
                    "worktree",
                    "add",
                    &worktree_path.to_string_lossy(),
                    branch_name,
                ])
                .current_dir(&workspace_root)
                .output()
                .context("failed to run git worktree add (existing branch)")?;

            if !output2.status.success() {
                let stderr2 = String::from_utf8_lossy(&output2.stderr);
                bail!("git worktree add failed: {}", stderr2.trim());
            }
        } else {
            bail!("git worktree add failed: {}", stderr.trim());
        }
    }

    Ok(worktree_path.to_string_lossy().to_string())
}

/// Merge a task's branch back into the current branch.
/// Returns a list of conflicting files if merge fails, or Ok(()) on success.
pub fn merge_branch(branch_name: &str) -> Result<MergeResult> {
    let workspace_root = std::env::current_dir()?;

    let output = Command::new("git")
        .args([
            "merge",
            "--no-ff",
            branch_name,
            "-m",
            &format!("omt: merge {branch_name}"),
        ])
        .current_dir(&workspace_root)
        .output()
        .context("failed to run git merge")?;

    if output.status.success() {
        return Ok(MergeResult::Success);
    }

    // Check for merge conflicts
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("CONFLICT") || stderr.contains("Automatic merge failed") {
        // List conflicting files
        let diff_output = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(&workspace_root)
            .output()?;

        let files: Vec<String> = String::from_utf8_lossy(&diff_output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();

        // Abort the merge so the workspace is clean
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(&workspace_root)
            .output();

        return Ok(MergeResult::Conflict(files));
    }

    bail!("git merge failed: {}", stderr.trim());
}

pub enum MergeResult {
    Success,
    Conflict(Vec<String>),
}

/// Remove a worktree and optionally its branch.
pub fn remove_worktree(worktree_path: &Path) -> Result<()> {
    let workspace_root = std::env::current_dir()?;

    let output = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(&workspace_root)
        .output()
        .context("failed to run git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree remove failed: {}", stderr.trim());
    }

    Ok(())
}

/// Delete an omt branch.
pub fn delete_branch(branch_name: &str) -> Result<()> {
    let workspace_root = std::env::current_dir()?;

    let output = Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(&workspace_root)
        .output()
        .context("failed to run git branch -D")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git branch -D failed: {}", stderr.trim());
    }

    Ok(())
}

/// Check if a worktree has uncommitted changes.
pub fn has_uncommitted_changes(worktree_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .context("failed to run git status")?;

    Ok(!output.stdout.is_empty())
}

/// Ensure `.omt` is in `.gitignore`.
fn ensure_gitignore(workspace_root: &Path) -> Result<()> {
    let gitignore = workspace_root.join(".gitignore");
    let content = std::fs::read_to_string(&gitignore).unwrap_or_default();

    if content
        .lines()
        .any(|line| line.trim() == ".omt" || line.trim() == ".omt/")
    {
        return Ok(());
    }

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)?;

    use std::io::Write;
    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, ".omt")?;

    Ok(())
}
