//! Which deferred tools a session has pulled into its tool list.
//!
//! Lives here rather than in `baybo-tools` because a session persists the set,
//! and `Session` cannot depend on the tool layer. Only the names travel this
//! far down; what a tool *is* stays in `baybo-tools`.

use serde::{Deserialize, Serialize};

/// The deferred tools one session has loaded, by name.
///
/// Ordered and deduplicated on construction. That is not tidiness: the tool
/// array is serialised ahead of the transcript, and the provider's cache keys
/// on the exact request prefix — so a set whose iteration order depended on
/// load order would move the cache boundary on every call.
///
/// Grow-only by construction: there is no removal. A session that loaded a
/// tool and then lost it would rewrite the array mid-conversation for no
/// reason the model could act on, and pay a full prefix miss to take a
/// capability away.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LoadedTools(Vec<String>);

impl LoadedTools {
    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|n| n == name)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Add `name`, returning whether this call was the one that added it — so
    /// a caller can persist and log exactly once per tool.
    pub fn insert(&mut self, name: impl Into<String>) -> bool {
        let name = name.into();
        if self.contains(&name) {
            return false;
        }
        self.0.push(name);
        self.0.sort();
        true
    }
}

impl<S: Into<String>> FromIterator<S> for LoadedTools {
    fn from_iter<T: IntoIterator<Item = S>>(iter: T) -> Self {
        let mut names: Vec<String> = iter.into_iter().map(Into::into).collect();
        names.sort();
        names.dedup();
        Self(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool array is sent ahead of the transcript, so its order is a
    /// prompt-cache boundary rather than a cosmetic detail.
    #[test]
    fn load_order_does_not_survive_into_the_set() {
        let a: LoadedTools = ["b", "a"].into_iter().collect();
        let b: LoadedTools = ["a", "b"].into_iter().collect();
        assert_eq!(a, b);

        let mut c = LoadedTools::default();
        c.insert("b");
        c.insert("a");
        assert_eq!(c, a);
    }

    #[test]
    fn inserting_twice_reports_the_second_as_a_no_op() {
        let mut set = LoadedTools::default();
        assert!(set.insert("browser/click"));
        assert!(!set.insert("browser/click"));
        assert_eq!(set.iter().count(), 1);
    }

    /// Persisted on the session row, so a row written before the field existed
    /// has to load as "nothing loaded" rather than fail.
    #[test]
    fn an_absent_field_deserialises_to_empty() {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            loaded_tools: LoadedTools,
        }
        let row: Row = serde_json::from_str("{}").expect("parse");
        assert!(row.loaded_tools.is_empty());
    }
}
