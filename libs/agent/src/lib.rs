pub fn name() -> &'static str {
    "agent"
}

pub fn status() -> &'static str {
    "ready"
}

#[cfg(test)]
mod tests {
    use super::{name, status};

    #[test]
    fn name_is_agent() {
        assert_eq!(name(), "agent");
    }

    #[test]
    fn status_is_ready() {
        assert_eq!(status(), "ready");
    }
}
