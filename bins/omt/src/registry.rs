//! omt agent registry — CLI-level management of known A2A agents.

use anyhow::Result;

use a2a::AgentRegistry;

/// Path to the omt agent registry file.
fn registry_path() -> std::path::PathBuf {
    // ~/.cache/omt/registry.json
    crate::state::runs_dir()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("registry.json")
}

/// Load the agent registry.
pub fn load() -> Result<AgentRegistry> {
    AgentRegistry::load(registry_path())
}

/// Register a remote agent by URL (fetches agent card).
pub async fn add_agent(endpoint: &str) -> Result<()> {
    let mut reg = load()?;
    let entry = reg.register(endpoint).await?;
    eprintln!(
        "registered: {} ({})",
        entry.card.name,
        entry.card.description.as_deref().unwrap_or("-")
    );
    let skills: Vec<&str> = entry.card.skills.iter().map(|s| s.name.as_str()).collect();
    if !skills.is_empty() {
        eprintln!("  skills: {}", skills.join(", "));
    }
    Ok(())
}

/// List all registered agents.
pub fn list_agents() -> Result<()> {
    let reg = load()?;
    let agents = reg.list();

    if agents.is_empty() {
        eprintln!("no agents registered. use `omt agent add <url>` to register one.");
        return Ok(());
    }

    eprintln!("── Registered Agents ({}) ──", agents.len());
    for entry in agents {
        let skills: Vec<&str> = entry
            .card
            .skills
            .iter()
            .flat_map(|s| s.tags.iter().map(|t| t.as_str()))
            .collect();
        let tags = if skills.is_empty() {
            String::new()
        } else {
            format!(" [{}]", skills.join(", "))
        };
        eprintln!(
            "  {} — {}{tags}",
            entry.card.name,
            entry.endpoint,
        );
        if let Some(desc) = &entry.card.description {
            eprintln!("    {desc}");
        }
    }
    Ok(())
}

/// Remove a registered agent by name.
pub fn remove_agent(name: &str) -> Result<()> {
    let mut reg = load()?;
    if reg.unregister(name)? {
        eprintln!("removed: {name}");
    } else {
        eprintln!("agent '{name}' not found");
    }
    Ok(())
}

/// Health-check all registered agents.
pub async fn check_agents() -> Result<()> {
    let reg = load()?;
    let agents = reg.list();

    if agents.is_empty() {
        eprintln!("no agents registered.");
        return Ok(());
    }

    for entry in agents {
        let ok = reg.health_check(&entry.card.name).await.unwrap_or(false);
        let status = if ok { "✓ ok" } else { "✗ unreachable" };
        eprintln!("  {} {} — {}", status, entry.card.name, entry.endpoint);
    }
    Ok(())
}
