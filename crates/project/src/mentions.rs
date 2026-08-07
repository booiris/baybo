//! Who a comment is addressed to.
//!
//! Pure, like the other rule modules here. The composer has to say what
//! sending will do *before* the request, so it reads the same rule the
//! manager applies.

use baybo_model::AgentHandle;

/// Every `@handle` in a comment, in the order written, deduplicated.
///
/// A handle must be preceded by whitespace or start the text, so an email
/// address or a path fragment is not a mention. The grammar is
/// [`AgentHandle`]'s, so the scan stops at the first character a handle
/// cannot contain — which is what lets `@dev-1's` and `@dev-1,` both name
/// `dev-1`.
pub(crate) fn mentions(text: &str) -> Vec<AgentHandle> {
    let mut found: Vec<AgentHandle> = Vec::new();
    let bytes = text.as_bytes();
    for (index, _) in text.match_indices('@') {
        let preceded_ok = index == 0
            || bytes
                .get(index - 1)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'(' || *b == b'@');
        if !preceded_ok {
            continue;
        }
        let rest = &text[index + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .unwrap_or(rest.len());
        // Trailing dashes are outside the grammar, so `@dev-` names `dev`.
        let candidate = rest[..end].trim_end_matches('-');
        if let Ok(handle) = AgentHandle::parse(candidate)
            && !found.contains(&handle)
        {
            found.push(handle);
        }
    }
    found
}

/// The handle a comment is addressed to, if it should change who is on the
/// card.
///
/// **Only when nobody is assigned.** An @mention on somebody else's card is
/// how one agent asks another a question; treating that as a reassignment
/// would let a passing remark take work away from whoever is doing it. The
/// spec's rule is for the unassigned case, where a mention is the operator
/// saying "you take this".
///
/// The *first* mention wins: "@dev-1 and @qa should look" names one owner
/// and asks a second to read, and picking the last would silently make the
/// aside the assignee.
pub(crate) fn assigns_to(assignee_is_set: bool, text: &str) -> Option<AgentHandle> {
    if assignee_is_set {
        return None;
    }
    mentions(text).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        mentions(text)
            .into_iter()
            .map(|h| h.as_str().to_owned())
            .collect()
    }

    #[test]
    fn a_mention_is_a_handle_after_whitespace() {
        assert_eq!(names("@dev-1 take this"), vec!["dev-1"]);
        assert_eq!(names("please @dev-1"), vec!["dev-1"]);
        assert_eq!(names("(@lead)"), vec!["lead"]);
    }

    #[test]
    fn punctuation_ends_a_handle_without_eating_it() {
        // The grammar stops the scan, so possessives and commas work.
        assert_eq!(names("@dev-1's branch"), vec!["dev-1"]);
        assert_eq!(names("@dev-1, @qa: look"), vec!["dev-1", "qa"]);
        assert_eq!(names("ask @dev-."), vec!["dev"]);
    }

    #[test]
    fn something_that_is_not_a_mention_is_not_one() {
        // An address and a path fragment both contain an `@` with a handle
        // after it, and neither is addressed to anybody.
        assert!(names("mail me at me@dev-1").is_empty());
        assert!(names("see docs/x@lead").is_empty());
        // …and an empty or ungrammatical handle names nobody.
        assert!(names("@ dev").is_empty());
        assert!(names("@Dev").is_empty());
        assert!(names("@1st").is_empty());
    }

    #[test]
    fn the_same_handle_twice_is_one_mention() {
        assert_eq!(names("@dev-1 please, @dev-1"), vec!["dev-1"]);
    }

    #[test]
    fn a_mention_assigns_only_when_nobody_is_on_the_card() {
        // On a card somebody is working, an @mention is one agent asking
        // another a question. Treating it as a reassignment would let a
        // passing remark take work away from whoever is doing it.
        assert!(assigns_to(true, "@dev-1 what do you think?").is_none());
        assert_eq!(
            assigns_to(false, "@dev-1 what do you think?").map(|h| h.as_str().to_owned()),
            Some("dev-1".to_owned())
        );
        assert!(assigns_to(false, "somebody should look").is_none());
    }

    #[test]
    fn the_first_mention_is_the_owner() {
        // "@dev-1 and @qa should look" names one owner and asks a second to
        // read; taking the last would silently make the aside the assignee.
        assert_eq!(
            assigns_to(false, "@dev-1 and @qa should look").map(|h| h.as_str().to_owned()),
            Some("dev-1".to_owned())
        );
    }
}
