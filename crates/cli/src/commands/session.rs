use std::path::PathBuf;

use baybo_model::{ChannelType, SessionId};
use baybo_query::QueryApi;
use baybo_session::StoredMessage;
use baybo_session::search::{SearchLimits, SearchResults, SessionSearch};
use baybo_store::SearchScope;
use serde_json::{Value, json};
use tokio::fs;

use crate::cli::{SessionCmd, SessionSearchArgs};
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

/// Width of one human-mode excerpt, in chars.
const SEARCH_EXCERPT_MAX_CHARS: usize = 120;
/// Chars of context kept before the matched term inside an excerpt.
const SEARCH_EXCERPT_LEAD_CHARS: usize = 40;
const UNTITLED: &str = "(untitled)";
const ELLIPSIS: &str = "…";

pub async fn handle(ctx: &CommandContext, cmd: SessionCmd) -> Result<CommandOutput> {
    match cmd {
        SessionCmd::List => list(ctx).await,
        SessionCmd::Show { id } => show(ctx, &id).await,
        SessionCmd::History {
            id,
            include_superseded,
            superseded_only,
        } => history(ctx, &id, include_superseded, superseded_only).await,
        SessionCmd::Search(args) => search(ctx, args).await,
        SessionCmd::Export { id, out, yes } => export(ctx, &id, out.as_deref(), yes).await,
    }
}

fn sessions(ctx: &CommandContext) -> Result<&baybo_agent::SessionManager> {
    ctx.session.as_deref().ok_or_else(|| {
        CliError::Manager("session manager is not available in this invocation".into())
    })
}

fn session_search(ctx: &CommandContext) -> Result<&SessionSearch> {
    ctx.session_search.as_deref().ok_or_else(|| {
        CliError::Manager("session search is not available in this invocation".into())
    })
}

async fn search(ctx: &CommandContext, args: SessionSearchArgs) -> Result<CommandOutput> {
    let service = session_search(ctx)?;
    let query = args.query.join(" ");
    let scope = SearchScope {
        channel: args.channel.as_deref().map(ChannelType::from),
        session: args.session.as_deref().map(SessionId::from),
        include_hidden: args.all || args.include_hidden,
        include_archived: args.all || args.include_archived,
        include_cron_workspaces: args.all || args.include_cron_workspaces,
    };
    let limits = SearchLimits {
        sessions: args.limit,
        hits_per_session: args.per_session,
        scan: args.scan,
    };
    let results = service
        .search(&query, &scope, limits)
        .await
        .map_err(|e| CliError::Manager(format!("search messages: {e}")))?;

    Ok(CommandOutput {
        human: render_search_human(&query, &scope, limits, &results),
        data: Some(render_search_json(&query, &scope, limits, &results)),
    })
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn scope_line(scope: &SearchScope) -> String {
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    format!(
        "scope: channel={}  session={}  hidden={}  archived={}  cron-workspaces={}",
        scope.channel.as_ref().map_or("any", ChannelType::as_str),
        scope.session.as_ref().map_or("any", SessionId::as_str),
        yes_no(scope.include_hidden),
        yes_no(scope.include_archived),
        yes_no(scope.include_cron_workspaces),
    )
}

/// The axes still closed, as the flags that would open them.
fn widening_flags(scope: &SearchScope) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if !scope.include_hidden {
        flags.push("--include-hidden");
    }
    if !scope.include_archived {
        flags.push("--include-archived");
    }
    if !scope.include_cron_workspaces {
        flags.push("--include-cron-workspaces");
    }
    flags
}

/// One line of context around the first query term. Collapses whitespace,
/// windows on char boundaries (never bytes, so CJK cannot split a codepoint),
/// and falls back to the head of the text when the typed term is not a
/// literal substring — prefix widening and NFC normalisation can both make
/// the index match something the typed bytes do not.
fn excerpt(text: &str, first_term: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= SEARCH_EXCERPT_MAX_CHARS {
        return flat;
    }
    // Folded one char at a time so `lower` stays index-aligned with `chars`:
    // `str::to_lowercase` can expand a char (U+0130 → `i` + U+0307) and shift
    // every later position.
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let lower: Vec<char> = chars.iter().map(|c| fold(*c)).collect();
    let term: Vec<char> = first_term.chars().map(fold).collect();
    let found = (!term.is_empty())
        .then(|| lower.windows(term.len()).position(|w| w == term.as_slice()))
        .flatten();
    let start = found
        .map(|at| at.saturating_sub(SEARCH_EXCERPT_LEAD_CHARS))
        .unwrap_or(0)
        .min(chars.len().saturating_sub(SEARCH_EXCERPT_MAX_CHARS));
    let end = (start + SEARCH_EXCERPT_MAX_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push_str(ELLIPSIS);
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push_str(ELLIPSIS);
    }
    out
}

fn render_search_human(
    query: &str,
    scope: &SearchScope,
    limits: SearchLimits,
    results: &SearchResults,
) -> String {
    let mut buf = String::new();
    if results.groups.is_empty() {
        buf.push_str(&format!("no matches for \"{query}\"\n"));
        buf.push_str(&scope_line(scope));
        let flags = widening_flags(scope);
        if !flags.is_empty() {
            buf.push_str(&format!(
                "\nhint: widen with {} (or --all)",
                flags.join(" / ")
            ));
        }
        return buf;
    }

    let total: usize = results.groups.iter().map(|g| g.total_hits).sum();
    let sessions = results.groups.len();
    let verb = if sessions == 1 { "matches" } else { "match" };
    buf.push_str(&format!(
        "{sessions} session{} {verb} \"{query}\" ({total} hit{})",
        plural(sessions),
        plural(total)
    ));
    if results.truncated {
        buf.push_str(&format!(
            " — scan window full ({} rows): narrow the query or raise --scan",
            limits.scan
        ));
    }
    if results.sessions_matched > sessions {
        buf.push_str(&format!(
            " — showing {sessions} of {}: raise -n",
            results.sessions_matched
        ));
    }
    buf.push('\n');
    buf.push_str(&scope_line(scope));
    buf.push_str("\n\n");

    let first_term = query.split_whitespace().next().unwrap_or_default();
    for group in &results.groups {
        buf.push_str(&format!(
            "{:<38}  {:<8}  {}  {} hit{}\n",
            group.session_id,
            group.channel,
            group.title.as_deref().unwrap_or(UNTITLED),
            group.total_hits,
            plural(group.total_hits),
        ));
        for hit in &group.hits {
            buf.push_str(&format!(
                "  #{:<5} {:<9}  {}  {}\n",
                hit.ordinal,
                hit.role.as_str(),
                hit.created_at.to_rfc3339(),
                excerpt(&hit.text, first_term),
            ));
        }
        let more = group.total_hits.saturating_sub(group.hits.len());
        if more > 0 {
            buf.push_str(&format!("  {ELLIPSIS} and {more} more\n"));
        }
    }
    buf.push_str(
        "\nfull text of a hit: baybo session history <id> --include-superseded --json   (rows carry \"ordinal\")",
    );
    buf
}

fn render_search_json(
    query: &str,
    scope: &SearchScope,
    limits: SearchLimits,
    results: &SearchResults,
) -> Value {
    let groups: Vec<Value> = results
        .groups
        .iter()
        .map(|g| {
            let hits: Vec<Value> = g
                .hits
                .iter()
                .map(|h| {
                    json!({
                        "ordinal": h.ordinal,
                        "role": h.role.as_str(),
                        "created_at": h.created_at.to_rfc3339(),
                        "score": h.score,
                        "superseded_by": h.superseded_by,
                        "text": h.text,
                    })
                })
                .collect();
            json!({
                "session_id": g.session_id,
                "channel": g.channel,
                "session_title": g.title,
                "total_hits": g.total_hits,
                "hits": hits,
            })
        })
        .collect();
    json!({
        "query": query,
        "scope": {
            "channel": scope.channel,
            "session": scope.session,
            "include_hidden": scope.include_hidden,
            "include_archived": scope.include_archived,
            "include_cron_workspaces": scope.include_cron_workspaces,
        },
        "limits": {
            "sessions": limits.sessions,
            "per_session": limits.hits_per_session,
            "scan": limits.scan,
        },
        "sessions_matched": results.sessions_matched,
        "truncated": results.truncated,
        "groups": groups,
    })
}

/// Trace counts are optional on `show` — argv-light boots that lack
/// `QueryApi` still surface metadata + message count gracefully. SQL
/// counts only; the full trace tree is never materialised for a tally.
async fn try_replay_counts(
    ctx: &CommandContext,
    session_id: &SessionId,
) -> Option<(usize, usize, usize)> {
    let api: &QueryApi = ctx.query_api.as_deref()?;
    api.trace_counts(session_id).await.ok()
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let sessions = mgr
        .list()
        .await
        .map_err(|e| CliError::Manager(format!("list sessions: {e}")))?;

    if sessions.is_empty() {
        return Ok(CommandOutput {
            human: "no sessions".to_string(),
            data: Some(json!({ "sessions": [] })),
        });
    }

    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "user": s.user.id,
                "channel": s.channel.to_string(),
                "last_active": s.last_active.to_rfc3339(),
            })
        })
        .collect();

    let mut human = String::from("id                                    channel   last_active\n");
    for s in &sessions {
        human.push_str(&format!(
            "{:<38}  {:<8}  {}\n",
            s.id,
            s.channel.to_string(),
            s.last_active.to_rfc3339(),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "sessions": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let typed = SessionId::from(id);
    let session = mgr
        .get(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("get session: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("session {id} not found")))?;
    let messages = mgr
        .load_active_session_messages(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("load context: {e}")))?;
    let trace_counts = try_replay_counts(ctx, &typed).await;

    let mut value = json!({
        "id": session.id.to_string(),
        "user": {
            "id": session.user.id,
            "name": session.user.name,
        },
        "channel": session.channel.to_string(),
        "created_at": session.created_at.to_rfc3339(),
        "last_active": session.last_active.to_rfc3339(),
        "messages": messages.len(),
        "called_skills": baybo_context::scan_skill_calls(&messages),
    });
    if let Some((turns, steps, spans)) = trace_counts
        && let Value::Object(ref mut map) = value
    {
        map.insert(
            "trace".into(),
            json!({ "turns": turns, "steps": steps, "spans": spans }),
        );
    }

    let called_skills = baybo_context::scan_skill_calls(&messages);
    let called_skills_human = if called_skills.is_empty() {
        "(none)".to_string()
    } else {
        called_skills.join(", ")
    };
    let trace_human = match trace_counts {
        Some((j, s, p)) => format!("\ntrace:          {j} turns · {s} steps · {p} spans"),
        None => String::new(),
    };

    let human = format!(
        "id:             {}\nuser:           {}\nchannel:        {}\ncreated:        {}\nlast_active:    {}\nmessages:       {}\ncalled_skills:  {}{}",
        session.id,
        session.user.id,
        session.channel,
        session.created_at.to_rfc3339(),
        session.last_active.to_rfc3339(),
        messages.len(),
        called_skills_human,
        trace_human,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn history(
    ctx: &CommandContext,
    id: &str,
    include_superseded: bool,
    superseded_only: bool,
) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let typed = SessionId::from(id);

    if !(include_superseded || superseded_only) {
        return active_history(mgr, id, &typed).await;
    }
    full_history(mgr, id, &typed, superseded_only).await
}

async fn active_history(
    mgr: &baybo_agent::SessionManager,
    id: &str,
    typed: &SessionId,
) -> Result<CommandOutput> {
    let messages = mgr
        .history(typed)
        .await
        .map_err(|e| CliError::Manager(format!("session history: {e}")))?;

    let count = messages.len();
    let value = serde_json::to_value(&messages)
        .map_err(|e| CliError::Serialization(format!("serialize messages: {e}")))?;

    let human = if count == 0 {
        format!("session {id}: no messages")
    } else {
        let mut buf = format!("session {id}: {count} messages\n");
        for (i, msg) in messages.iter().enumerate() {
            buf.push_str(&format!(
                "  [{i}] {:?} · {} blocks\n",
                msg.role,
                msg.content.len()
            ));
        }
        buf.trim_end().to_string()
    };

    Ok(CommandOutput {
        human,
        data: Some(json!({ "session": id, "messages": value })),
    })
}

/// Walk the full message log including compaction-superseded rows.
/// Each row carries `superseded_by` — `None` means active, `Some(n)`
/// means a later compaction at ordinal `n` replaced this row.
async fn full_history(
    mgr: &baybo_agent::SessionManager,
    id: &str,
    typed: &SessionId,
    superseded_only: bool,
) -> Result<CommandOutput> {
    let all = mgr
        .load_session_messages_with_supersede(typed)
        .await
        .map_err(|e| CliError::Manager(format!("session history: {e}")))?;
    let filtered: Vec<&StoredMessage> = if superseded_only {
        all.iter().filter(|m| m.superseded_by.is_some()).collect()
    } else {
        all.iter().collect()
    };

    if filtered.is_empty() {
        let human = if superseded_only {
            format!("session {id}: no superseded messages")
        } else {
            format!("session {id}: no messages")
        };
        return Ok(CommandOutput {
            human,
            data: Some(json!({ "session": id, "messages": [] })),
        });
    }

    let active_count = all.iter().filter(|m| m.superseded_by.is_none()).count();
    let total = all.len();
    let superseded_count = total - active_count;

    let header = if superseded_only {
        let plural = if filtered.len() == 1 { "" } else { "s" };
        format!(
            "session {id}: {} superseded message{plural}",
            filtered.len()
        )
    } else {
        format!(
            "session {id}: {total} messages ({active_count} active, {superseded_count} superseded)"
        )
    };

    let mut buf = format!("{header}\n");
    for m in &filtered {
        let marker = match m.superseded_by {
            Some(n) => format!("[→ #{n}]"),
            None => "[active]".into(),
        };
        buf.push_str(&format!(
            "  [#{}] {:?} · {} blocks · {marker}\n",
            m.ordinal,
            m.message.role,
            m.message.content.len()
        ));
    }

    let rows: Vec<Value> = filtered
        .iter()
        .map(|m| {
            json!({
                "ordinal": m.ordinal,
                "superseded_by": m.superseded_by,
                "message": m.message,
            })
        })
        .collect();

    Ok(CommandOutput {
        human: buf.trim_end().to_string(),
        data: Some(json!({ "session": id, "messages": rows })),
    })
}

async fn export(
    ctx: &CommandContext,
    id: &str,
    out: Option<&str>,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && out.is_some() && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would write trace for session {id} to {}; re-run with --yes to confirm",
            out.unwrap_or("(stdout)")
        )));
    }

    let api = ctx
        .query_api
        .as_deref()
        .ok_or_else(|| CliError::Manager("query api is not available in this invocation".into()))?;
    let session_id = SessionId::from(id);
    let replay = api
        .replay(&session_id, None)
        .await
        .map_err(|e| CliError::Manager(format!("replay session: {e}")))?;
    if replay.turns.is_empty() {
        return Err(CliError::Manager(format!(
            "no trace recorded for session {id}"
        )));
    }

    let tree: Vec<Value> = replay
        .turns
        .iter()
        .map(|rj| {
            let steps: Vec<Value> = rj
                .steps
                .iter()
                .map(|rs| json!({ "step": rs.step, "spans": rs.spans }))
                .collect();
            json!({ "turn": rj.turn, "steps": steps })
        })
        .collect();
    let exported = json!({ "session": id, "turns": tree });
    let json_text = serde_json::to_string_pretty(&exported)
        .map_err(|e| CliError::Manager(format!("serialize trace: {e}")))?;

    if let Some(path_str) = out {
        let path = PathBuf::from(path_str);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| CliError::Manager(format!("create parent {parent:?}: {e}")))?;
        }
        fs::write(&path, json_text.as_bytes())
            .await
            .map_err(|e| CliError::Manager(format!("write {path:?}: {e}")))?;
        return Ok(CommandOutput {
            human: format!("wrote trace for session {id} to {path_str}"),
            data: Some(json!({
                "session": id,
                "path": path_str,
                "bytes": json_text.len(),
            })),
        });
    }

    Ok(CommandOutput {
        human: json_text.clone(),
        data: Some(exported),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::Role;
    use baybo_session::search::{SessionSearchGroup, SessionSearchHit};
    use chrono::{TimeZone, Utc};

    fn hit(ordinal: i64, text: &str) -> SessionSearchHit {
        SessionSearchHit {
            ordinal,
            role: Role::User,
            text: text.to_owned(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            superseded_by: None,
            score: -1.5,
        }
    }

    fn results(groups: Vec<SessionSearchGroup>, truncated: bool) -> SearchResults {
        SearchResults {
            sessions_matched: groups.len(),
            groups,
            truncated,
        }
    }

    #[test]
    fn excerpt_collapses_whitespace_and_keeps_short_text_whole() {
        assert_eq!(excerpt("a  b\n\tc", "b"), "a b c");
    }

    #[test]
    fn excerpt_windows_around_the_term_on_char_boundaries() {
        let text = format!("{}数据库的迁移{}", "前".repeat(200), "后".repeat(200));
        let out = excerpt(&text, "迁移");
        assert!(
            out.starts_with(ELLIPSIS) && out.ends_with(ELLIPSIS),
            "{out}"
        );
        assert!(out.contains("数据库的迁移"), "{out}");
        let body: String = out.trim_matches(|c| c == '…').to_owned();
        assert_eq!(body.chars().count(), SEARCH_EXCERPT_MAX_CHARS);
        let term_at = body
            .chars()
            .collect::<Vec<_>>()
            .windows(2)
            .position(|w| w == ['迁', '移'])
            .expect("term inside the window");
        assert_eq!(term_at, SEARCH_EXCERPT_LEAD_CHARS);
    }

    #[test]
    fn excerpt_matches_case_insensitively() {
        let text = format!("{} Deploy script {}", "x".repeat(100), "y".repeat(100));
        let out = excerpt(&text, "deploy");
        assert!(out.contains("Deploy script"), "{out}");
    }

    #[test]
    fn excerpt_stays_aligned_past_chars_that_expand_when_lowercased() {
        let text = format!("{}needle{}", "İ".repeat(100), "y".repeat(100));
        let out = excerpt(&text, "NEEDLE");
        assert!(out.contains("needle"), "{out}");
    }

    #[test]
    fn excerpt_falls_back_to_the_head_when_the_term_is_absent() {
        let text = "z".repeat(300);
        let out = excerpt(&text, "nothing");
        assert!(!out.starts_with(ELLIPSIS));
        assert!(out.ends_with(ELLIPSIS));
        assert_eq!(out.chars().count(), SEARCH_EXCERPT_MAX_CHARS + 1);
    }

    #[test]
    fn scope_line_reports_every_axis() {
        let scope = SearchScope {
            channel: Some(ChannelType::owner()),
            session: Some(SessionId::from("s1")),
            include_hidden: true,
            include_archived: false,
            include_cron_workspaces: false,
        };
        assert_eq!(
            scope_line(&scope),
            "scope: channel=owner  session=s1  hidden=yes  archived=no  cron-workspaces=no"
        );
        assert_eq!(
            scope_line(&SearchScope::default()),
            "scope: channel=any  session=any  hidden=no  archived=no  cron-workspaces=no"
        );
    }

    #[test]
    fn human_render_prints_full_ids_ordinals_counts_and_footer() {
        let groups = vec![
            SessionSearchGroup {
                session_id: SessionId::from("01J8Q0ABCDEFGHJKMNPQRSTVWXYZ"),
                channel: ChannelType::owner(),
                title: Some("Migrating the schema".into()),
                hits: vec![hit(12, "数据库的迁移怎么做"), hit(15, "second")],
                total_hits: 3,
            },
            SessionSearchGroup {
                session_id: SessionId::from("tg-1"),
                channel: ChannelType::telegram(),
                title: None,
                hits: vec![hit(0, "once")],
                total_hits: 1,
            },
        ];
        let out = render_search_human(
            "迁移",
            &SearchScope::default(),
            SearchLimits::default(),
            &results(groups, false),
        );
        assert!(
            out.starts_with("2 sessions match \"迁移\" (4 hits)\n"),
            "{out}"
        );
        assert!(out.contains("01J8Q0ABCDEFGHJKMNPQRSTVWXYZ"), "{out}");
        assert!(out.contains("Migrating the schema  3 hits"), "{out}");
        assert!(
            out.contains("  #12    user       2023-11-14T22:13:20+00:00  数据库的迁移怎么做"),
            "{out}"
        );
        assert!(out.contains("  … and 1 more\n"), "{out}");
        assert!(out.contains("tg-1"), "{out}");
        assert!(out.contains("telegram  (untitled)  1 hit\n"), "{out}");
        assert!(out.ends_with("(rows carry \"ordinal\")"), "{out}");
        assert!(!out.contains("scan window"), "{out}");
        assert!(!out.contains("showing"), "{out}");
    }

    #[test]
    fn human_render_flags_a_session_cap_cut() {
        let groups = vec![SessionSearchGroup {
            session_id: SessionId::from("s"),
            channel: ChannelType::owner(),
            title: None,
            hits: vec![hit(0, "x")],
            total_hits: 1,
        }];
        let results = SearchResults {
            groups,
            sessions_matched: 7,
            truncated: false,
        };
        let limits = SearchLimits {
            sessions: 1,
            ..SearchLimits::default()
        };
        let out = render_search_human("x", &SearchScope::default(), limits, &results);
        assert!(out.contains("— showing 1 of 7: raise -n"), "{out}");
    }

    #[test]
    fn human_render_flags_a_full_scan_window() {
        let groups = vec![SessionSearchGroup {
            session_id: SessionId::from("s"),
            channel: ChannelType::owner(),
            title: None,
            hits: vec![hit(0, "x")],
            total_hits: 1,
        }];
        let limits = SearchLimits {
            scan: 7,
            ..SearchLimits::default()
        };
        let out = render_search_human("x", &SearchScope::default(), limits, &results(groups, true));
        assert!(
            out.contains("scan window full (7 rows): narrow the query or raise --scan"),
            "{out}"
        );
    }

    #[test]
    fn empty_render_hints_only_the_closed_axes() {
        let scope = SearchScope {
            include_hidden: true,
            ..SearchScope::default()
        };
        let out = render_search_human(
            "nothing",
            &scope,
            SearchLimits::default(),
            &results(vec![], false),
        );
        assert!(out.starts_with("no matches for \"nothing\"\n"), "{out}");
        assert!(
            out.ends_with(
                "hint: widen with --include-archived / --include-cron-workspaces (or --all)"
            ),
            "{out}"
        );

        let all = SearchScope {
            include_hidden: true,
            include_archived: true,
            include_cron_workspaces: true,
            ..SearchScope::default()
        };
        let out = render_search_human(
            "nothing",
            &all,
            SearchLimits::default(),
            &results(vec![], false),
        );
        assert!(!out.contains("hint:"), "{out}");
    }

    #[test]
    fn json_render_keeps_every_key_present() {
        let groups = vec![SessionSearchGroup {
            session_id: SessionId::from("s"),
            channel: ChannelType::owner(),
            title: None,
            hits: vec![hit(4, "full prose")],
            total_hits: 1,
        }];
        let value = render_search_json(
            "q",
            &SearchScope::default(),
            SearchLimits::default(),
            &results(groups, false),
        );
        assert_eq!(value["query"], "q");
        assert!(value["scope"]["channel"].is_null());
        assert!(value["scope"]["session"].is_null());
        assert_eq!(value["scope"]["include_hidden"], false);
        assert_eq!(value["limits"]["sessions"], 20);
        assert_eq!(value["limits"]["per_session"], 3);
        assert_eq!(value["limits"]["scan"], 300);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["sessions_matched"], 1);
        let group = &value["groups"][0];
        assert_eq!(group["session_id"], "s");
        assert_eq!(group["channel"], "owner");
        assert!(group["session_title"].is_null());
        assert_eq!(group["total_hits"], 1);
        let hit = &group["hits"][0];
        assert_eq!(hit["ordinal"], 4);
        assert_eq!(hit["role"], "user");
        assert_eq!(hit["text"], "full prose");
        assert!(hit["superseded_by"].is_null());
        assert_eq!(hit["score"], -1.5);
    }
}
