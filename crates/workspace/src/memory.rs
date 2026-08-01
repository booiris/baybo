//! The agent-authored memory tree: one markdown file per remembered
//! fact, plus the `MEMORY.md` index the system prompt carries verbatim.
//!
//! This module owns only the *addresses and seeding* of that tree. The
//! prompt framing that teaches the model how to use it lives in
//! `baybo-context`, and the write path is the ordinary `Edit` / `Write`
//! tooling — nothing here interprets a memory file's contents.
//!
//! One tree per agent, at `<root>/personas/<id>/memory/` — the built-in
//! included, because it is just another persona directory (see
//! [`crate::paths::WorkspacePaths`]).

use std::path::Path;

use crate::identity::read_or_seed;
use crate::prompt::MEMORY_INDEX_TEMPLATE;

/// Read a memory index, seeding an empty one if the file is absent.
///
/// Auto-seeding mirrors the identity files: a deleted `MEMORY.md` is
/// recreated on the next assembly rather than leaving the system prompt
/// half-formed, and the parent directory is created on the way, so a
/// fresh workspace (or a brand-new agent) has somewhere to write its
/// first memory without any other setup step.
pub async fn load_memory_index(path: &Path) -> anyhow::Result<String> {
    read_or_seed(path, MEMORY_INDEX_TEMPLATE)
        .await
        .map_err(|e| anyhow::anyhow!("load memory index {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::WorkspacePaths;

    #[tokio::test]
    async fn seeds_an_empty_index_and_its_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        let index = load_memory_index(&paths.persona_memory_index_file("baybo"))
            .await
            .expect("load");

        assert_eq!(index, MEMORY_INDEX_TEMPLATE);
        assert!(paths.persona_memory_dir("baybo").is_dir());
        assert!(paths.persona_memory_index_file("baybo").is_file());
    }

    #[tokio::test]
    async fn preserves_an_existing_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        const WRITTEN: &str = "# Memory Index\n\n- [Cat's name](cat-name.md) — Mochi\n";

        tokio::fs::create_dir_all(paths.persona_memory_dir("baybo"))
            .await
            .expect("mkdir");
        tokio::fs::write(paths.persona_memory_index_file("baybo"), WRITTEN)
            .await
            .expect("write");

        let index = load_memory_index(&paths.persona_memory_index_file("baybo"))
            .await
            .expect("load");
        assert_eq!(index, WRITTEN);
    }

    #[tokio::test]
    async fn per_agent_index_lands_under_that_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        load_memory_index(&paths.persona_memory_index_file("agt_7"))
            .await
            .expect("load");

        assert!(paths.persona_memory_dir("agt_7").is_dir());
        // Another agent's tree is untouched — memory is partitioned, with
        // no shared fallback for one agent's writes to land in.
        assert!(!paths.persona_memory_dir("baybo").exists());
    }
}
