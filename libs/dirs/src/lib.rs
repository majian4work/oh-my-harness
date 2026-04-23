use std::path::PathBuf;

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// `$XDG_CACHE_HOME/omh` or `~/.cache/omh`
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("omh");
    }
    home_dir().join(".cache/omh")
}

/// `$XDG_CONFIG_HOME/omh` or `~/.config/omh`
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("omh");
    }
    home_dir().join(".config/omh")
}

/// `$XDG_CACHE_HOME/omh/logs` or `~/.cache/omh/logs`
pub fn log_dir() -> PathBuf {
    cache_dir().join("logs")
}

/// `$XDG_CACHE_HOME/omh/sessions` or `~/.cache/omh/sessions`
pub fn sessions_dir() -> PathBuf {
    cache_dir().join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_under_home() {
        // With no XDG vars set, paths should fall under $HOME
        let home = home_dir();
        assert_eq!(cache_dir(), home.join(".cache/omh"));
        assert_eq!(config_dir(), home.join(".config/omh"));
        assert_eq!(log_dir(), home.join(".cache/omh/logs"));
        assert_eq!(sessions_dir(), home.join(".cache/omh/sessions"));
    }
}
