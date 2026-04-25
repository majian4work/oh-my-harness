use super::input_ast::SlashInvocation;

pub fn parse_slash_invocation(input: &str) -> Option<SlashInvocation> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed[1..].splitn(2, ' ');
    let command = parts.next().unwrap_or_default().trim();
    if command.is_empty() {
        return None;
    }

    Some(SlashInvocation {
        raw: trimmed.to_string(),
        command: command.to_string(),
        args: parts.next().unwrap_or_default().trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_slash_invocation;

    #[test]
    fn parses_slash_command_and_args() {
        let parsed = parse_slash_invocation(" /models refresh ").expect("slash command");

        assert_eq!(parsed.raw, "/models refresh");
        assert_eq!(parsed.command, "models");
        assert_eq!(parsed.args, "refresh");
    }

    #[test]
    fn returns_none_for_non_slash_input() {
        assert!(parse_slash_invocation("hello world").is_none());
        assert!(parse_slash_invocation("").is_none());
        assert!(parse_slash_invocation("  ").is_none());
    }

    #[test]
    fn returns_none_for_bare_slash() {
        assert!(parse_slash_invocation("/").is_none());
        assert!(parse_slash_invocation("/ ").is_none());
    }

    #[test]
    fn parses_command_without_args() {
        let parsed = parse_slash_invocation("/skills").expect("slash command");

        assert_eq!(parsed.command, "skills");
        assert_eq!(parsed.args, "");
    }
}
