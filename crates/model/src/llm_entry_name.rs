//! Newtype for an LLM config entry name (`llm[*].name` in `aura.json`).
//!
//! The same operator-defined key threads through config (`default-llm`,
//! `model_tiers`), the runtime `LlmClientPool` (entry keys, tier map),
//! and the subagent spawn path (`initial_llm`). Wrapping it stops a
//! tier→entry binding from being confused with any other string and
//! documents intent at every signature. Serialization is transparent,
//! so on-disk `aura.json` and the admin API wire shape stay plain
//! strings.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmEntryName(String);

impl LlmEntryName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LlmEntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegate to the inner string so `{:?}` renders `"name"` rather
        // than `LlmEntryName("name")` — operator-facing error messages
        // (validation, boot, CLI) format the entry name with `{:?}`.
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for LlmEntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for LlmEntryName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for LlmEntryName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl PartialEq<&str> for LlmEntryName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for LlmEntryName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}
