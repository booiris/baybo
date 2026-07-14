# Slash Commands — Account Authorization

## Problem

Slash commands (`/config set`, `/config unset`, `/secret delete`, `/pair approve`, …) are privileged operator actions: they mutate workspace state, edit config, and move secrets and pairings around. Operator slash dispatch (`CliSlashHandler`) is currently unwired from every chat surface: sidecar channels only reach the gateway dispatcher (`crates/gateway/src/channel/slash.rs`, which owns `/new` only) and sit behind the per-user pairing gate (`docs/modules/pairing.md`), and the TUI exposes only local session commands. But pairing authorizes who may *talk to a bot*, not who may run *operator commands* — the moment `CliSlashHandler` is wired to a multi-user channel (Telegram, Discord, future group chats), **every paired sender can drive operator commands**, not just the workspace owner. This doc covers that gap.

Identity does exist at the wire level: `Frame::Message` carries `user_id` + `bot_id`, and the gateway's sidecar dispatcher already takes the caller — `try_handle(resolver, content, channel_type, user_id)` (`crates/gateway/src/channel/slash.rs:69`). None of it reaches the operator seam. On the `Message` side the identity is the `sender: User` field (`crates/channels/src/types.rs:12`), and that field never reaches the slash dispatcher: `SlashHandler::handle(&self, raw: &str)` (`crates/channels/src/slash.rs:120`) — the trait `CliSlashHandler` implements — takes only the raw line. The TUI adapter intercepts `/` lines *before* building the `Message` at all (`crates/tui/src/lib.rs` `handle_slash` — slash lookup happens on the input line pre-Message), so even if the trait were widened, channel adapters still have to be audited to actually pass the sender in.

There is also no config-side notion of "which account is authorized." No `allowed_users`, no owner id, nothing cryptographically bound to a channel principal.

## Why it's blocked

1. **Sender identity does not reach `SlashHandler`.** The trait signature needs to grow a principal argument (or an auth context struct) so the dispatcher can see *who* invoked the command, not just *what*. This is a cross-crate surface change: `baybo-channels`, every adapter, and `baybo-cli`'s `CliSlashHandler` all move together.

2. **No authorization model in config.** There is no `BayboConfig` section declaring the workspace owner, nor per-channel `allowed_users` lists. The config-wiring batch that used to carry the other missing sections has since shipped, so the `access` schema has to be added to `baybo-config` on its own, with validation like the other sections.

3. **`CommandContext::invocation` is too coarse.** It distinguishes `Argv` vs `Slash` but has no principal. Handlers that want to gate on "this is the owner" have nowhere to read it from. The `confirmed: bool` field follows the same pattern — an auth principal slot next to it is the natural shape.

4. **CLI/local client vs chat channels need different defaults.** A local `baybo` CLI invocation is implicitly authorized (the process runs as the operator); a Telegram message is not. Any design has to let the local client (the TUI adapter) inject a "local owner" principal without making chat adapters silently inherit that trust.

## Proposed direction

Ship in three stages; the enforcement lands last.

### Stage 1 — principal on the dispatch path

Widen `SlashHandler::handle` to accept a principal:

```rust
pub struct SlashPrincipal {
    pub channel: ChannelType,
    pub user_id: String,       // stable per-channel id (e.g. Telegram user id)
    pub display_name: Option<String>,
    pub source: PrincipalSource, // Cli (local process), Channel (chat adapter), Cron (scheduler)
}

#[async_trait]
pub trait SlashHandler {
    async fn handle(&self, raw: &str, principal: &SlashPrincipal) -> SlashOutcome;
}
```

Every channel adapter that currently calls `SlashHandler::handle` is updated to build a `SlashPrincipal` from its `sender: User`. The local client (the TUI adapter) passes `PrincipalSource::Cli` with the process's `USER`.

Thread the principal into `CommandContext` alongside `invocation` so handlers can read it.

### Stage 2 — config schema

New `config.access` section:

```toml
[access]
# Slash commands are accepted from these principals only.
# Empty => deny all chat-originated slash. CLI (local) principals always allowed.
[[access.allowed]]
channel = "telegram"
user_id = "123456789"

[[access.allowed]]
channel = "discord"
user_id = "987654321"
```

Validated in `baybo-config::validate` the same way other allow-list sections are. Exposed as `Arc<AccessPolicy>` on `CommandContext`.

### Stage 3 — enforcement

In `CliSlashHandler::try_dispatch` (`crates/cli/src/slash.rs:30`), before `dispatch::run`, consult the `AccessPolicy`:

- `PrincipalSource::Cli` → always allowed (local process trust).
- `PrincipalSource::Channel` → must match an `[[access.allowed]]` row for `(channel, user_id)`. Otherwise return a new `CliError::Unauthorized` rendered as a terse `"not authorized"` outcome — no command enumeration, no hint about the allowlist.
- `PrincipalSource::Cron` → allowed if the scheduled job's owner is in the allowlist at schedule time; revalidated at execution.

Record every rejection through the recorder with the principal fields redacted to a hash (`docs/modules/` observability rule: placeholders and summaries, no raw identifiers).

## Design constraints

- **Fail closed.** An empty or missing `[access]` section must reject chat-originated slash commands, not allow them. A "wide-open by default" knob is a foot-gun on a framework that also ships a Telegram adapter.
- **CLI path stays usable without config.** The local operator running `baybo` from their shell must not need to write an allowlist entry to use slash commands. The `PrincipalSource::Cli` branch exists for this.
- **No leak via error messages.** Unauthorized senders get one flat message; do not echo the attempted command back. The rejection path is also a side-channel — rate-limit or swallow entirely if the channel is known-spammy.
- **Secrets redaction still applies.** Principal ids in logs/traces follow the existing sanitized-placeholder rule; do not dump raw Telegram user ids into trace output.
- **Cron revalidation.** A scheduled slash command must re-check authorization at fire time, not just at schedule time — the allowlist can shrink between the two.

## Related

- `crates/channels/src/slash.rs:116-130` — `SlashHandler` trait, needs principal arg
- `crates/channels/src/types.rs:12` — `Message.sender` is the identity source
- `crates/cli/src/slash.rs:30` — `CliSlashHandler::try_dispatch`, enforcement point
- `crates/cli/src/context.rs` — `CommandContext`, add `principal` + `access_policy`
- `docs/modules/cli.md` §"Slash Integration" — update once enforcement ships
