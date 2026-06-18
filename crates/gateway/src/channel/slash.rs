//! Gateway-side slash-command dispatcher for sidecar channels.
//!
//! Sidecars (Telegram, WeChat, …) don't carry their own slash handler:
//! whatever the user types reaches the gateway as a normal
//! [`Frame::Message`]. A handful of commands need server-side state
//! (the `(channel_type, user_id) → session_id` mapping in particular),
//! so the gateway intercepts them right after the pairing gate and
//! before [`ChannelSessionResolver::resolve_or_create`].
//!
//! `/new` is wired here — it forces a fresh aura session for the
//! calling user and replies with a confirmation. `/compact` and `/stop`
//! are published in the manifest so sidecars register them natively (e.g.
//! Telegram's `setMyCommands`) but are **not** intercepted: the
//! dispatcher returns `PassThrough` so the message flows through to the
//! agent runtime — `/compact` to the actor (compression pass) and `/stop`
//! to the `Router` (cancel the in-flight turn + subagents out-of-band).
//! Trailing arguments are ignored. Matching is case-insensitive on the
//! command token.
//!
//! Adapter-side commands (TUI's `/clear`, `/quit`, …) live in their
//! respective channels and never reach the gateway.

use std::sync::LazyLock;

use aura_channels::wire::{Message as WireMessage, SlashCommandSpec};
use aura_channels::{
    COMPACT_COMMAND_NAME, GOAL_COMMAND_DESCRIPTION, GOAL_COMMAND_NAME, STOP_COMMAND_DESCRIPTION,
    STOP_COMMAND_NAME,
};
use aura_model::{ChannelType, SessionId};

use super::session_resolver::ChannelSessionResolver;

/// Authoritative manifest of slash commands the gateway dispatcher
/// recognises for sidecar channels. Pushed to every freshly-registered
/// sidecar via [`aura_channels::wire::Frame::SlashManifest`] so each
/// platform's native command surface (Telegram `setMyCommands`,
/// Discord application commands, …) stays in sync without sidecars
/// keeping their own hardcoded copy. Adding a new command here is the
/// single edit needed for the dispatcher + every sidecar to learn it.
static MANIFEST: LazyLock<Vec<SlashCommandSpec>> = LazyLock::new(|| {
    vec![
        SlashCommandSpec {
            command: "new".to_string(),
            description: "Start a fresh session".to_string(),
        },
        SlashCommandSpec {
            command: COMPACT_COMMAND_NAME.to_string(),
            description: "Summarize the conversation and free context".to_string(),
        },
        SlashCommandSpec {
            command: STOP_COMMAND_NAME.to_string(),
            description: STOP_COMMAND_DESCRIPTION.to_string(),
        },
        SlashCommandSpec {
            command: GOAL_COMMAND_NAME.to_string(),
            description: GOAL_COMMAND_DESCRIPTION.to_string(),
        },
    ]
});

pub fn manifest() -> Vec<SlashCommandSpec> {
    MANIFEST.clone()
}

pub(crate) enum SlashOutcome {
    /// Command was recognised and executed; the wrapped frame is the
    /// reply to send back through the sidecar. The caller MUST skip
    /// session resolution and router intake for this inbound.
    Handled(WireMessage),
    /// Not a slash command (or not one we own). The caller should
    /// continue with the normal inbound path.
    PassThrough,
}

pub(crate) async fn try_handle(
    resolver: &ChannelSessionResolver,
    content: &str,
    channel_type: &ChannelType,
    user_id: &str,
) -> SlashOutcome {
    let trimmed = content.trim();
    // Diagnostic dump for "why didn't my /command fire?" reports. The
    // hex of the first 32 bytes catches the usual culprits: a full-
    // width `／` (U+FF0F → `ef bc 8f`), a BOM (`ef bb bf`), a zero-
    // width space (`e2 80 8b`), or any leading whitespace the
    // sidecar's text extractor failed to strip. Off by default at
    // `info` so grep is enough to find a single `/new` invocation.
    let head_hex: String = trimmed
        .as_bytes()
        .iter()
        .take(32)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::debug!(
        %channel_type,
        len = trimmed.len(),
        head_hex = %head_hex,
        "slash dispatch entry",
    );
    let Some(rest) = trimmed.strip_prefix('/') else {
        return SlashOutcome::PassThrough;
    };
    let Some(token) = rest.split_whitespace().next() else {
        return SlashOutcome::PassThrough;
    };
    // Telegram group chats invoke commands as `/new@BotName` so a
    // single bot in the group picks up the call. The sidecar forwards
    // `message.text` verbatim, so strip the optional `@<bot>` suffix
    // before matching.
    let cmd = token.split('@').next().unwrap_or("");

    match cmd.to_ascii_lowercase().as_str() {
        "new" => SlashOutcome::Handled(handle_new(resolver, channel_type, user_id).await),
        _ => SlashOutcome::PassThrough,
    }
}

async fn handle_new(
    resolver: &ChannelSessionResolver,
    channel_type: &ChannelType,
    user_id: &str,
) -> WireMessage {
    match resolver.reset_session(channel_type, user_id).await {
        Ok(session_id) => reply(
            channel_type,
            user_id,
            &session_id,
            "Started a fresh session.",
        ),
        Err(e) => {
            tracing::warn!(
                error = %e,
                %channel_type,
                "/new failed to reset session"
            );
            reply(
                channel_type,
                user_id,
                &SessionId::from(""),
                &format!("Failed to start new session: {e}"),
            )
        }
    }
}

fn reply(
    channel_type: &ChannelType,
    user_id: &str,
    session_id: &SessionId,
    text: &str,
) -> WireMessage {
    WireMessage {
        content: text.to_owned(),
        session_id: session_id.clone(),
        user_id: user_id.to_owned(),
        channel_type: channel_type.clone(),
        // Outbound `bot_id` is empty by convention (`agent_output_to_frame`
        // does the same): sidecars recover the bot from their own
        // `user_id → bot_id` map.
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
        role: aura_channels::MessageRole::Assistant,
        ordinal: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aura_agent::SessionManager;
    use aura_storage::libsql::{LibsqlChannelSessionStore, LibsqlPool, LibsqlSessionStore};

    use super::*;

    async fn build() -> ChannelSessionResolver {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let session_store = Arc::new(LibsqlSessionStore::new(pool.clone()));
        let summary_store = Arc::new(aura_storage::libsql::LibsqlSessionSummaryStore::new(
            pool.clone(),
        ));
        let session_mgr = Arc::new(SessionManager::new(session_store, summary_store));
        let channel_store = Arc::new(LibsqlChannelSessionStore::new(pool));
        ChannelSessionResolver::new(session_mgr, channel_store)
    }

    fn assert_passthrough(outcome: SlashOutcome) {
        match outcome {
            SlashOutcome::PassThrough => {}
            SlashOutcome::Handled(m) => panic!("expected PassThrough, got Handled({m:?})"),
        }
    }

    fn assert_handled(outcome: SlashOutcome) -> WireMessage {
        match outcome {
            SlashOutcome::Handled(m) => m,
            SlashOutcome::PassThrough => panic!("expected Handled, got PassThrough"),
        }
    }

    #[tokio::test]
    async fn plain_text_passes_through() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        assert_passthrough(try_handle(&resolver, "hello world", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn unknown_command_passes_through() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        assert_passthrough(try_handle(&resolver, "/unknown", &ct, "tg_1").await);
        assert_passthrough(try_handle(&resolver, "/newsletter", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn slash_in_middle_passes_through() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        assert_passthrough(try_handle(&resolver, "see /new for help", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn empty_slash_passes_through() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        assert_passthrough(try_handle(&resolver, "/", &ct, "tg_1").await);
        assert_passthrough(try_handle(&resolver, "/   ", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn new_creates_session_and_repoints_mapping() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let original = resolver.resolve_or_create(&ct, "tg_1").await.unwrap();

        let reply = assert_handled(try_handle(&resolver, "/new", &ct, "tg_1").await);
        assert_eq!(reply.user_id, "tg_1");
        assert_eq!(reply.channel_type, ct);
        assert!(reply.bot_id.is_empty());
        assert!(reply.content.contains("fresh session"));
        assert_ne!(reply.session_id, "");
        assert_ne!(reply.session_id, original);

        let after = resolver.resolve_or_create(&ct, "tg_1").await.unwrap();
        assert_eq!(after, reply.session_id);
    }

    #[tokio::test]
    async fn new_accepts_telegram_bot_suffix() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let original = resolver.resolve_or_create(&ct, "tg_1").await.unwrap();

        let reply = assert_handled(try_handle(&resolver, "/new@MyBot", &ct, "tg_1").await);
        assert!(reply.content.contains("fresh session"));

        // Suffix + args + casing all together.
        let reply2 = assert_handled(try_handle(&resolver, "/NeW@MyBot extra", &ct, "tg_1").await);
        assert!(reply2.content.contains("fresh session"));
        assert_ne!(reply2.session_id, original);
        // Bot suffix on a non-command stays a non-command.
        assert_passthrough(try_handle(&resolver, "/newsletter@MyBot", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn new_is_case_insensitive_and_ignores_args() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let _ = resolver.resolve_or_create(&ct, "tg_1").await.unwrap();

        let reply = assert_handled(try_handle(&resolver, "  /NeW  whatever", &ct, "tg_1").await);
        assert!(reply.content.contains("fresh session"));
    }

    #[tokio::test]
    async fn compact_is_advertised_in_manifest() {
        let cmds: Vec<String> = manifest().into_iter().map(|c| c.command).collect();
        assert!(cmds.iter().any(|c| c == COMPACT_COMMAND_NAME));
        assert!(cmds.iter().any(|c| c == "new"));
    }

    #[tokio::test]
    async fn compact_passes_through_to_actor() {
        // The gateway dispatcher only owns commands that need
        // server-side state (`/new` repoints the mapping). `/compact`
        // operates on the live session and is handled by the actor,
        // so the dispatcher must let it through verbatim.
        let resolver = build().await;
        let ct = ChannelType::telegram();
        assert_passthrough(try_handle(&resolver, "/compact", &ct, "tg_1").await);
        assert_passthrough(try_handle(&resolver, "/CompAct@MyBot extra", &ct, "tg_1").await);
    }

    #[tokio::test]
    async fn new_works_without_prior_mapping() {
        let resolver = build().await;
        let ct = ChannelType::telegram();
        let reply = assert_handled(try_handle(&resolver, "/new", &ct, "tg_brand_new").await);
        assert!(reply.content.contains("fresh session"));
        let resolved = resolver
            .resolve_or_create(&ct, "tg_brand_new")
            .await
            .unwrap();
        assert_eq!(resolved, reply.session_id);
    }
}
