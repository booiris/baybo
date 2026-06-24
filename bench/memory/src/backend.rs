//! Backend glue, all through the backends' concrete (context-free) methods in
//! `baybo_memory::backends` — no `MemoryContext`, no Baybo agent/trace/tool stack.
//! A [`BackendHandle`] unifies the two real backends behind the three things the
//! harness needs: ingest a conversation, settle extraction to true completion,
//! and recall (read-only).
//!
//! - **openviking**: writes per message (`add_message`), commits per session
//!   (`commit_session`) and polls each extraction task (`wait_commit_task`).
//! - **mem0**: writes per turn-pair (`add_turn`, extraction is per-add) and
//!   polls the account-global events feed (`wait_events_completed`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use baybo_memory::RecalledMemory;
use baybo_memory::backends::mem0::{Mem0Config, Mem0Memory};
use baybo_memory::backends::openviking::{OpenVikingConfig, OpenVikingMemory};

use crate::testset::{BenchConversation, BenchSample};
use crate::{ConvScope, MEM0_API_KEY_ENV, scope_session_id};

/// Which real backend an arm drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Mem0,
    OpenViking,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Mem0 => "mem0",
            Backend::OpenViking => "openviking",
        }
    }
}

/// Connection settings resolved from flags + env by the bins.
pub struct BackendOpts {
    pub top_k: usize,
    pub mem0_base_url: Option<String>,
    pub openviking_endpoint: Option<String>,
    pub openviking_account: Option<String>,
}

/// A constructed backend, sharable across concurrent QA tasks (each variant
/// holds an `Arc`).
pub enum BackendHandle {
    Mem0(Arc<Mem0Memory>),
    OpenViking(Arc<OpenVikingMemory>),
}

/// Build the selected backend. mem0 needs `MEM0_API_KEY`; openviking needs an
/// endpoint (flag or `OPENVIKING_ENDPOINT`) and an optional `OPENVIKING_API_KEY`
/// (empty = unauthenticated local dev).
pub fn build_backend(backend: Backend, opts: &BackendOpts) -> Result<BackendHandle> {
    match backend {
        Backend::Mem0 => {
            // A mem0 base URL means a self-hosted OSS server (no key needed);
            // its absence means the managed cloud Platform (key required).
            let self_hosted = opts.mem0_base_url.is_some();
            let api_key = if self_hosted {
                std::env::var(MEM0_API_KEY_ENV).unwrap_or_default()
            } else {
                std::env::var(MEM0_API_KEY_ENV)
                    .with_context(|| format!("{MEM0_API_KEY_ENV} must be set for the mem0 arm"))?
            };
            let cfg = Mem0Config {
                api_key_name: None,
                base_url: opts.mem0_base_url.clone(),
                rerank: Some(true),
                top_k: Some(opts.top_k),
                self_hosted: Some(self_hosted),
            };
            Ok(BackendHandle::Mem0(Arc::new(
                Mem0Memory::new(cfg, api_key, None).context("construct mem0 backend")?,
            )))
        }
        Backend::OpenViking => {
            let endpoint = opts
                .openviking_endpoint
                .clone()
                .or_else(|| std::env::var("OPENVIKING_ENDPOINT").ok())
                .context(
                    "openviking endpoint required (--openviking-endpoint or OPENVIKING_ENDPOINT)",
                )?;
            let key = std::env::var("OPENVIKING_API_KEY").unwrap_or_default();
            let cfg = OpenVikingConfig {
                endpoint: Some(endpoint),
                api_key_name: None,
                account: opts.openviking_account.clone(),
                top_k: Some(opts.top_k),
            };
            Ok(BackendHandle::OpenViking(Arc::new(
                OpenVikingMemory::new(cfg, key, None).context("construct openviking backend")?,
            )))
        }
    }
}

impl BackendHandle {
    pub fn backend(&self) -> Backend {
        match self {
            BackendHandle::Mem0(_) => Backend::Mem0,
            BackendHandle::OpenViking(_) => Backend::OpenViking,
        }
    }

    /// QA recall — read-only `recall_for` on the concrete backend. This is the
    /// ONLY memory access on the QA path, so a QA turn can never write the
    /// question into the store.
    pub async fn recall_for(&self, user_id: &str, query: &str) -> Result<Vec<RecalledMemory>> {
        let memories = match self {
            BackendHandle::Mem0(m) => m.recall_for(user_id, query).await?,
            BackendHandle::OpenViking(o) => o.recall_for(user_id, query).await?,
        };
        Ok(memories)
    }

    /// Ingest one conversation through the backend's concrete write methods.
    /// Returns the session ids openviking will commit (empty for mem0).
    pub async fn ingest_conversation(
        &self,
        user_id: &str,
        conv_idx: usize,
        sample: &BenchSample,
    ) -> Result<ConvScope> {
        let mut session_ids = Vec::new();
        let mut pairs_total = 0usize;
        for session in &sample.conversation.sessions {
            let pairs = session.turn_pairs();
            match self {
                BackendHandle::OpenViking(ov) => {
                    let sid = scope_session_id(user_id, session.index);
                    for (user_text, assistant_text) in &pairs {
                        if !user_text.is_empty() {
                            ov.add_message(user_id, &sid, "user", user_text)
                                .await
                                .with_context(|| {
                                    format!(
                                        "add user msg (conv {conv_idx}, session {})",
                                        session.index
                                    )
                                })?;
                        }
                        if !assistant_text.is_empty() {
                            ov.add_message(user_id, &sid, "assistant", assistant_text)
                                .await
                                .with_context(|| {
                                    format!(
                                        "add assistant msg (conv {conv_idx}, session {})",
                                        session.index
                                    )
                                })?;
                        }
                    }
                    session_ids.push(sid);
                }
                BackendHandle::Mem0(m) => {
                    for (user_text, assistant_text) in &pairs {
                        m.add_turn(user_id, user_text, assistant_text)
                            .await
                            .with_context(|| {
                                format!("add turn (conv {conv_idx}, session {})", session.index)
                            })?;
                    }
                }
            }
            pairs_total += pairs.len();
        }
        Ok(ConvScope {
            conv_idx,
            sample_id: sample.sample_id.clone(),
            user_id: user_id.to_string(),
            session_ids,
            turn_pairs: pairs_total,
            memories_stored: 0,
        })
    }

    /// Wait for true server-side extraction completion, then one recall for a
    /// meaningful count. openviking polls each commit task; mem0 polls the
    /// account-global events feed.
    pub async fn settle(
        &self,
        user_id: &str,
        session_ids: &[String],
        probe: &str,
        opts: &SettleOpts,
    ) -> Result<SettleOutcome> {
        let completed = match self {
            BackendHandle::OpenViking(ov) => {
                commit_and_wait(ov, user_id, session_ids, opts).await?
            }
            BackendHandle::Mem0(m) => m.wait_events_completed(opts.interval, opts.timeout).await,
        };
        let count = self
            .recall_for(user_id, probe)
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        Ok(SettleOutcome {
            count,
            stabilized: completed && count > 0,
        })
    }
}

/// Settle knobs (poll cadence + per-task / overall ceiling for the wait).
pub struct SettleOpts {
    pub timeout: Duration,
    pub interval: Duration,
}

/// Outcome of the settle step: `count` memories surface for the probe, and
/// `stabilized` is whether extraction reached true completion (openviking:
/// every commit task `completed`; mem0: every add-event `completed`). A
/// non-`stabilized` outcome (or `count == 0`) marks the manifest unsettled, and
/// QA refuses it unless overridden.
pub struct SettleOutcome {
    pub count: usize,
    pub stabilized: bool,
}

/// openviking: commit every session, capturing each commit's extraction
/// `task_id`, then poll those tasks to a terminal state. Returns whether EVERY
/// task reached `completed`. Commits fire first (extraction overlaps
/// server-side), then we poll; a synchronous commit (no `task_id`) needs no poll.
async fn commit_and_wait(
    ov: &OpenVikingMemory,
    user_id: &str,
    session_ids: &[String],
    opts: &SettleOpts,
) -> Result<bool> {
    let mut pending = Vec::new();
    for sid in session_ids {
        let ack = ov
            .commit_session(user_id, sid)
            .await
            .with_context(|| format!("openviking commit {sid}"))?;
        if ack.status != "completed"
            && let Some(task_id) = ack.task_id
        {
            pending.push((sid.clone(), task_id));
        }
    }
    let mut all_completed = true;
    for (sid, task_id) in &pending {
        let outcome = ov
            .wait_commit_task(user_id, task_id, opts.interval, opts.timeout)
            .await;
        if outcome.status != "completed" {
            all_completed = false;
            tracing::warn!(
                session = %sid,
                task = %task_id,
                status = %outcome.status,
                "openviking extraction task did not complete before timeout"
            );
        }
    }
    Ok(all_completed)
}

/// A probe query for the post-settle recall count, derived from the
/// conversation's first turn so it matches something the backend should have
/// extracted.
pub fn settle_probe(conversation: &BenchConversation) -> String {
    conversation
        .first_turn()
        .map(|t| t.chars().take(120).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "conversation".to_string())
}
