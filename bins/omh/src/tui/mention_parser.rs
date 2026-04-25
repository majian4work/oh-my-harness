use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub raw: String,
    pub body: String,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMentionQuery {
    pub typed_prefix: String,
    pub replace_range: Range<usize>,
}

pub fn scan_mentions(input: &str) -> Vec<MentionCandidate> {
    let mut mentions = Vec::new();

    for (index, ch) in input.char_indices() {
        if ch != '@' || !is_mention_boundary(input, index) {
            continue;
        }

        let end = mention_end(input, index);
        let raw = &input[index..end];
        let body = &input[index + 1..end];

        mentions.push(MentionCandidate {
            raw: raw.to_string(),
            body: body.to_string(),
            span: index..end,
        });
    }

    mentions
}

pub fn active_mention_query(input: &str, cursor_position: usize) -> Option<ActiveMentionQuery> {
    if cursor_position > input.len() {
        return None;
    }

    let prefix = input.get(..cursor_position)?;
    let at_pos = prefix.rfind('@')?;

    if !is_mention_boundary(input, at_pos) {
        return None;
    }

    let typed_prefix = &input[at_pos..cursor_position];
    if typed_prefix.chars().skip(1).any(|ch| ch.is_whitespace()) {
        return None;
    }

    let end = mention_end(input, at_pos);
    Some(ActiveMentionQuery {
        typed_prefix: typed_prefix.to_string(),
        replace_range: at_pos..end,
    })
}

fn is_mention_boundary(input: &str, index: usize) -> bool {
    index == 0
        || input[..index]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace())
}

fn mention_end(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(offset, _)| start + offset)
        .unwrap_or(input.len())
}

#[cfg(test)]
mod tests {
    use super::{active_mention_query, scan_mentions};

    #[test]
    fn scans_whitespace_delimited_mentions() {
        let mentions = scan_mentions("Ask @explore about @src/main.rs");

        assert_eq!(mentions.len(), 2);
        assert_eq!(mentions[0].raw, "@explore");
        assert_eq!(mentions[1].raw, "@src/main.rs");
    }

    #[test]
    fn ignores_embedded_at_signs() {
        let mentions = scan_mentions("email@example.com is not a mention");

        assert!(mentions.is_empty());
    }

    #[test]
    fn keeps_standalone_at_signs_for_validation() {
        let mentions = scan_mentions("ask @ to inspect");

        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].raw, "@");
        assert!(mentions[0].body.is_empty());
    }

    #[test]
    fn returns_active_query_replace_range() {
        let input = "Ask @expl";
        let query = active_mention_query(input, input.len()).expect("missing active mention");

        assert_eq!(query.typed_prefix, "@expl");
        assert_eq!(&input[query.replace_range], "@expl");
    }

    #[test]
    fn returns_active_query_replace_range_for_file_reference_mentions() {
        let input = "Ask about @src/main.rs";
        let query = active_mention_query(input, input.len()).expect("missing active mention");

        assert_eq!(query.typed_prefix, "@src/main.rs");
        assert_eq!(&input[query.replace_range], "@src/main.rs");
    }
}
