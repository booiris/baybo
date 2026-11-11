//! Grouped transcript and title search: the one composer of
//! `MessageSearchStore` and `SessionStore::session_titles`. Both
//! `GET /v1/chat/search` and `baybo session search` call it, so the scan
//! window, the grouping and merge rules and the title batch have exactly one
//! home. See `docs/search.md`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use baybo_model::{ChannelType, ContentBlock, Role, SessionId};
use baybo_store::{MessageSearchStore, SearchHit, SearchScope, SessionStore, TitleHit};
use chrono::{DateTime, Utc};

use crate::Result;

/// Enough conversations to fill a results panel without scrolling into a second
/// page.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
pub const MAX_SEARCH_LIMIT: usize = 50;

/// Raw hits pulled from the index before grouping.
///
/// Grouping has to happen over a window wider than the response, or one chatty
/// conversation buries every other. Measured on real data, `codex` matches 47
/// times across 17 conversations — but a flat top-30 covers only 7 of them,
/// because a single conversation takes 15 of the 30 slots. Client-side grouping
/// cannot fix that: the other 10 conversations were never sent.
///
/// Scanning wide is nearly free. Query cost tracks the number of MATCHES, not
/// the limit — `ORDER BY` visits every match before it drops any — so this
/// bounds the rows carried back through the trait, not the work sqlite does.
pub const SEARCH_SCAN_LIMIT: u32 = 300;

/// Excerpts shown per conversation. Past a few, a result card stops being
/// glanceable and the conversation is better opened than previewed.
pub const MAX_HITS_PER_SESSION: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct SearchLimits {
    /// Conversations returned. `search` clamps it to `MAX_SEARCH_LIMIT`; the
    /// CLI additionally rejects a larger value at parse time so an operator
    /// learns the ceiling instead of silently getting fewer rows.
    pub sessions: usize,
    /// Hits carried per conversation; `total_hits` still counts every match.
    pub hits_per_session: usize,
    /// Raw index rows scanned before grouping, applied to the message scan and
    /// the title scan separately; `truncated` is set when either window fills.
    pub scan: u32,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            sessions: DEFAULT_SEARCH_LIMIT,
            hits_per_session: MAX_HITS_PER_SESSION,
            scan: SEARCH_SCAN_LIMIT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub ordinal: i64,
    pub role: Role,
    /// Every `ContentBlock::Text` of the message joined with a newline — the
    /// ORIGINAL prose, never the segmented index text, so a caller highlighting
    /// by substring agrees with what the index matched.
    pub text: String,
    pub created_at: DateTime<Utc>,
    /// Copied from `SearchHit::superseded_by`, with the same caveat: it says
    /// the model's context was rewritten after this row. It is not a jump
    /// target and not an "off screen" marker.
    pub superseded_by: Option<i64>,
    /// FTS5 bm25 score; more negative is a better match.
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct SessionSearchGroup {
    pub session_id: SessionId,
    /// From the first message hit's `SearchHit::channel` (an index-time
    /// snapshot); for a title-only group, from `TitleHit::channel`.
    pub channel: ChannelType,
    pub title: Option<String>,
    /// The session's title matched the query. `title` is then the matched
    /// title, to highlight by substring like a hit's prose.
    pub title_match: bool,
    /// Message hits only, newest first, at most `SearchLimits::hits_per_session`.
    /// Empty for a group that matched on its title alone.
    pub hits: Vec<SessionSearchHit>,
    /// Every message match in the scan window, including those past the
    /// per-session cap; a title match does not count. Exact unless
    /// `SearchResults::truncated`.
    pub total_hits: usize,
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    /// Newest conversation first: a message-only conversation dates from its
    /// newest matching message, a title-matched one from its last activity.
    pub groups: Vec<SessionSearchGroup>,
    /// Distinct conversations the two scan windows held before
    /// `SearchLimits::sessions` cut the list; `groups.len()` when nothing was
    /// cut.
    pub sessions_matched: usize,
    /// True when either scan window filled, so some conversations may be
    /// missing and `total_hits` may undercount.
    pub truncated: bool,
}

pub struct SessionSearch {
    message_search: Arc<dyn MessageSearchStore>,
    sessions: Arc<dyn SessionStore>,
}

impl SessionSearch {
    pub fn new(
        message_search: Arc<dyn MessageSearchStore>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            message_search,
            sessions,
        }
    }

    /// Sessions within `scope` whose title or prose matches `query`, newest
    /// first (see `group_hits` for what dates a conversation).
    pub async fn search(
        &self,
        query: &str,
        scope: &SearchScope,
        limits: SearchLimits,
    ) -> Result<SearchResults> {
        let (hits, title_hits) = futures::try_join!(
            self.message_search
                .search_messages(query, scope, limits.scan),
            self.message_search.search_titles(query, scope, limits.scan),
        )?;
        let window = limits.scan as usize;
        let truncated = hits.len() == window || title_hits.len() == window;
        let (mut groups, sessions_matched) = group_hits(title_hits, hits, limits);

        // A title-matched group already carries its title. The rest are fetched
        // AFTER the cut, so the scan window's width costs no extra lookups —
        // and in one grouped flat-column query instead of a point lookup per
        // result conversation. A title batch this build cannot load is a
        // missing subtitle, not a failed search: degrade to empty rather than
        // fail the whole result set.
        let ids: Vec<SessionId> = groups
            .iter()
            .filter(|g| !g.title_match)
            .map(|g| g.session_id.clone())
            .collect();
        if !ids.is_empty() {
            let mut titles: HashMap<SessionId, Option<String>> =
                match self.sessions.session_titles(&ids).await {
                    Ok(rows) => rows.into_iter().collect(),
                    Err(e) => {
                        tracing::warn!(error = %e, "search: session titles unavailable");
                        HashMap::new()
                    }
                };
            for group in groups.iter_mut().filter(|g| !g.title_match) {
                group.title = titles.remove(&group.session_id).flatten();
            }
        }
        Ok(SearchResults {
            groups,
            sessions_matched,
            truncated,
        })
    }
}

/// Newest first, across both windows. A message-only conversation dates from
/// its newest matching message; a title-matched one from its `last_active`,
/// because a title names the whole conversation — and since no message is
/// newer than the conversation's last activity, that one rule covers a
/// conversation matched by both. Ties keep arrival order (the sort is stable).
///
/// Returns the cut list and the number of distinct conversations before the
/// cut.
fn group_hits(
    titles: Vec<TitleHit>,
    hits: Vec<SearchHit>,
    limits: SearchLimits,
) -> (Vec<SessionSearchGroup>, usize) {
    let mut newest: HashMap<SessionId, DateTime<Utc>> = HashMap::new();
    for hit in &hits {
        newest
            .entry(hit.session_id.clone())
            .or_insert(hit.created_at);
    }
    let mut dated: Vec<(DateTime<Utc>, SessionSearchGroup)> = group_message_hits(hits, limits)
        .into_iter()
        .map(|g| (newest.get(&g.session_id).copied().unwrap_or_default(), g))
        .collect();
    let position: HashMap<SessionId, usize> = dated
        .iter()
        .enumerate()
        .map(|(i, (_, g))| (g.session_id.clone(), i))
        .collect();
    let mut seen: HashSet<SessionId> = HashSet::new();
    for title in titles {
        if !seen.insert(title.session_id.clone()) {
            continue;
        }
        match position.get(&title.session_id) {
            Some(&i) => {
                let (at, group) = &mut dated[i];
                *at = (*at).max(title.last_active);
                group.title = Some(title.title);
                group.title_match = true;
            }
            None => dated.push((
                title.last_active,
                SessionSearchGroup {
                    session_id: title.session_id,
                    channel: title.channel,
                    title: Some(title.title),
                    title_match: true,
                    hits: Vec::new(),
                    total_hits: 0,
                },
            )),
        }
    }
    dated.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let matched = dated.len();
    let groups = dated
        .into_iter()
        .map(|(_, g)| g)
        .take(limits.sessions.min(MAX_SEARCH_LIMIT))
        .collect();
    (groups, matched)
}

/// Hits arrive newest first, so pushing each onto its group in arrival order
/// leaves every group's hits newest first and its first hit its newest.
fn group_message_hits(hits: Vec<SearchHit>, limits: SearchLimits) -> Vec<SessionSearchGroup> {
    let mut groups: Vec<SessionSearchGroup> = Vec::new();
    let mut index: HashMap<SessionId, usize> = HashMap::new();
    for hit in hits {
        let slot = match index.get(&hit.session_id) {
            Some(&i) => i,
            None => {
                index.insert(hit.session_id.clone(), groups.len());
                groups.push(SessionSearchGroup {
                    session_id: hit.session_id.clone(),
                    channel: hit.channel.clone(),
                    title: None,
                    title_match: false,
                    hits: Vec::new(),
                    total_hits: 0,
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[slot];
        // Count every match but carry only the newest few: `total_hits` is what
        // lets a caller say "and 12 more" without a second call.
        group.total_hits += 1;
        if group.hits.len() >= limits.hits_per_session {
            continue;
        }
        group.hits.push(SessionSearchHit {
            ordinal: hit.ordinal,
            role: hit.message.role,
            text: message_text(&hit.message.content),
            created_at: hit.created_at,
            superseded_by: hit.superseded_by,
            score: hit.score,
        });
    }
    groups
}

fn message_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemorySessionStore;
    use async_trait::async_trait;
    use baybo_model::{ChatMessage, Session, User};
    use parking_lot::Mutex;

    struct FakeMessageSearchStore {
        hits: Vec<SearchHit>,
        titles: Vec<TitleHit>,
        seen_limit: Mutex<Option<u32>>,
        seen_title_limit: Mutex<Option<u32>>,
    }

    impl FakeMessageSearchStore {
        fn new(hits: Vec<SearchHit>, titles: Vec<TitleHit>) -> Self {
            Self {
                hits,
                titles,
                seen_limit: Mutex::new(None),
                seen_title_limit: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl MessageSearchStore for FakeMessageSearchStore {
        async fn search_messages(
            &self,
            _query: &str,
            _scope: &SearchScope,
            limit: u32,
        ) -> baybo_store::message_search::Result<Vec<SearchHit>> {
            *self.seen_limit.lock() = Some(limit);
            Ok(self.hits.iter().take(limit as usize).cloned().collect())
        }

        async fn search_titles(
            &self,
            _query: &str,
            _scope: &SearchScope,
            limit: u32,
        ) -> baybo_store::message_search::Result<Vec<TitleHit>> {
            *self.seen_title_limit.lock() = Some(limit);
            Ok(self.titles.iter().take(limit as usize).cloned().collect())
        }
    }

    /// A fixed clock: `Utc::now()` per fixture would make each later fixture
    /// newer and turn arrival order upside down.
    fn minutes_ago(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap_or_default()
            - chrono::Duration::minutes(minutes)
    }

    fn title(session: &str, channel: &str, text: &str, active_minutes_ago: i64) -> TitleHit {
        TitleHit {
            session_id: SessionId::from(session),
            channel: ChannelType::from(channel),
            title: text.to_owned(),
            last_active: minutes_ago(active_minutes_ago),
            score: -1.0,
        }
    }

    /// Dated `ordinal` minutes ago, so ascending ordinals arrive newest first
    /// as the store returns them.
    fn hit(session: &str, ordinal: i64, channel: &str, text: &str) -> SearchHit {
        SearchHit {
            session_id: SessionId::from(session),
            ordinal,
            channel: ChannelType::from(channel),
            created_at: minutes_ago(ordinal),
            superseded_by: None,
            message: ChatMessage::user(vec![ContentBlock::Text(text.to_owned())]),
            score: -(ordinal as f64),
        }
    }

    fn repeated(session: &str, n: usize) -> Vec<SearchHit> {
        (0..n)
            .map(|i| hit(session, i as i64, "owner", &format!("检索 第{i}条")))
            .collect()
    }

    fn service(
        hits: Vec<SearchHit>,
    ) -> (
        SessionSearch,
        Arc<MemorySessionStore>,
        Arc<FakeMessageSearchStore>,
    ) {
        service_with_titles(hits, Vec::new())
    }

    fn service_with_titles(
        hits: Vec<SearchHit>,
        titles: Vec<TitleHit>,
    ) -> (
        SessionSearch,
        Arc<MemorySessionStore>,
        Arc<FakeMessageSearchStore>,
    ) {
        let sessions = Arc::new(MemorySessionStore::new());
        let fake = Arc::new(FakeMessageSearchStore::new(hits, titles));
        let search = SessionSearch::new(
            Arc::clone(&fake) as Arc<dyn MessageSearchStore>,
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
        );
        (search, sessions, fake)
    }

    #[test]
    fn groups_in_arrival_order_and_counts_past_the_cap() {
        let mut hits = repeated("chatty", 12);
        hits.extend(repeated("quiet-a", 1));
        hits.extend(repeated("quiet-b", 1));
        let (groups, matched) = group_hits(Vec::new(), hits, SearchLimits::default());

        assert_eq!(matched, 3);
        let ids: Vec<&str> = groups.iter().map(|g| g.session_id.as_str()).collect();
        assert_eq!(ids, ["chatty", "quiet-a", "quiet-b"]);
        assert_eq!(groups[0].total_hits, 12);
        assert_eq!(groups[0].hits.len(), MAX_HITS_PER_SESSION);
        assert_eq!(groups[1].total_hits, 1);
        assert_eq!(groups[1].hits.len(), 1);
    }

    #[test]
    fn hits_per_session_is_a_knob() {
        let (groups, _) = group_hits(
            Vec::new(),
            repeated("chatty", 5),
            SearchLimits {
                hits_per_session: 5,
                ..SearchLimits::default()
            },
        );
        assert_eq!(groups[0].hits.len(), 5);
        assert_eq!(groups[0].total_hits, 5);
    }

    #[test]
    fn session_limit_is_clamped_to_max_search_limit() {
        let hits: Vec<SearchHit> = (0..(MAX_SEARCH_LIMIT + 10))
            .map(|i| hit(&format!("s{i}"), 0, "owner", "x"))
            .collect();
        let (groups, matched) = group_hits(
            Vec::new(),
            hits,
            SearchLimits {
                sessions: 999,
                ..SearchLimits::default()
            },
        );
        assert_eq!(groups.len(), MAX_SEARCH_LIMIT);
        assert_eq!(matched, MAX_SEARCH_LIMIT + 10);
    }

    #[test]
    fn channel_comes_from_the_hit_and_text_joins_text_blocks() {
        let mut h = hit("s", 3, "telegram", "ignored");
        h.message = ChatMessage::assistant(vec![
            ContentBlock::Text("first".into()),
            ContentBlock::Text("second".into()),
        ]);
        let (groups, _) = group_hits(Vec::new(), vec![h], SearchLimits::default());
        assert_eq!(groups[0].channel.as_str(), "telegram");
        assert_eq!(groups[0].hits[0].text, "first\nsecond");
        assert_eq!(groups[0].hits[0].role, Role::Assistant);
        assert_eq!(groups[0].hits[0].ordinal, 3);
    }

    #[tokio::test]
    async fn truncated_when_scan_window_is_full_and_scan_is_passed_through() {
        let (search, _, fake) = service(repeated("s", 12));
        let limits = SearchLimits {
            scan: 10,
            ..SearchLimits::default()
        };
        let results = search
            .search("检索", &SearchScope::default(), limits)
            .await
            .unwrap();
        assert_eq!(*fake.seen_limit.lock(), Some(10));
        assert!(results.truncated);
        assert_eq!(results.groups.len(), 1);
        assert_eq!(results.groups[0].total_hits, 10);

        let (search, _, _) = service(repeated("s", 4));
        let results = search
            .search("检索", &SearchScope::default(), limits)
            .await
            .unwrap();
        assert!(!results.truncated);
    }

    #[tokio::test]
    async fn sessions_matched_counts_past_the_session_cap() {
        let (search, _, _) = service(vec![
            hit("a", 0, "owner", "x"),
            hit("b", 0, "owner", "x"),
            hit("c", 0, "owner", "x"),
        ]);
        let results = search
            .search(
                "x",
                &SearchScope::default(),
                SearchLimits {
                    sessions: 2,
                    ..SearchLimits::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(results.groups.len(), 2);
        assert_eq!(results.sessions_matched, 3);
    }

    #[tokio::test]
    async fn a_failing_title_batch_degrades_to_untitled_results() {
        let (search, sessions, _) = service(vec![hit("s", 0, "owner", "x")]);
        sessions.fail_titles(true);
        let results = search
            .search("x", &SearchScope::default(), SearchLimits::default())
            .await
            .expect("a missing subtitle is not a failed search");
        assert_eq!(results.groups.len(), 1);
        assert_eq!(results.groups[0].title, None);
    }

    #[tokio::test]
    async fn titles_are_attached_when_the_session_has_one() {
        let (search, sessions, _) = service(vec![
            hit("titled", 0, "owner", "x"),
            hit("untitled", 0, "owner", "x"),
        ]);
        for id in ["titled", "untitled"] {
            let now = Utc::now();
            sessions.seed_session(&Session {
                id: SessionId::from(id),
                user: User {
                    id: "u".into(),
                    name: None,
                    channel: ChannelType::owner(),
                },
                channel: ChannelType::owner(),
                created_at: now,
                last_active: now,
                state: baybo_model::SessionState::default(),
                root_session_id: SessionId::from(id),
                trigger: baybo_model::TriggerSource::User,
                lineage: None,
                hidden: false,
                pinned: false,
                archived: false,
                folder_id: None,
                title: None,
            });
        }
        sessions
            .set_title(&SessionId::from("titled"), Some("Migrating the schema"))
            .await
            .unwrap();

        let results = search
            .search("x", &SearchScope::default(), SearchLimits::default())
            .await
            .unwrap();
        assert_eq!(
            results.groups[0].title.as_deref(),
            Some("Migrating the schema")
        );
        assert_eq!(results.groups[1].title, None);
    }
    /// Conversations interleave by date across both windows: a message-only
    /// one by its newest hit, a title-matched one by its last activity. A
    /// session matching both is one group carrying its message hits.
    #[test]
    fn groups_are_newest_first_and_a_session_matching_both_is_one_group() {
        let hits = vec![
            hit("a", 1, "owner", "检索 a"),
            hit("both", 3, "owner", "检索 both"),
            hit("b", 5, "owner", "检索 b"),
            hit("both", 9, "owner", "检索 both again"),
        ];
        let titles = vec![
            title("only-title", "telegram", "另一个检索", 4),
            title("both", "owner", "检索的标题", 0),
        ];
        let (groups, matched) = group_hits(titles, hits, SearchLimits::default());

        let ids: Vec<&str> = groups.iter().map(|g| g.session_id.as_str()).collect();
        assert_eq!(ids, ["both", "a", "only-title", "b"]);
        assert_eq!(matched, 4, "a session matching both counts once");

        let both = &groups[0];
        assert!(both.title_match);
        assert_eq!(both.title.as_deref(), Some("检索的标题"));
        assert_eq!(both.total_hits, 2, "message hits ride along");
        let ordinals: Vec<i64> = both.hits.iter().map(|h| h.ordinal).collect();
        assert_eq!(ordinals, [3, 9], "hits stay newest first");

        let only = &groups[2];
        assert!(only.title_match);
        assert!(only.hits.is_empty());
        assert_eq!(only.total_hits, 0, "a title match is not a message hit");
        assert_eq!(only.channel.as_str(), "telegram");
        assert_eq!(only.title.as_deref(), Some("另一个检索"));

        assert!(!groups[1].title_match);
        assert_eq!(groups[1].title, None);
    }

    /// A stale title match does not outrank a fresh message match: titles get
    /// no tier of their own.
    #[test]
    fn an_old_title_match_sorts_below_a_newer_message_match() {
        let (groups, _) = group_hits(
            vec![title("old", "owner", "x", 60)],
            vec![hit("fresh", 2, "owner", "x")],
            SearchLimits::default(),
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.session_id.as_str()).collect();
        assert_eq!(ids, ["fresh", "old"]);
    }

    #[test]
    fn a_repeated_title_hit_does_not_duplicate_its_group() {
        let titles = vec![title("s", "owner", "t", 0), title("s", "owner", "t", 0)];
        let (groups, matched) = group_hits(titles, Vec::new(), SearchLimits::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(matched, 1);
    }

    #[test]
    fn the_session_cut_applies_to_the_merged_list() {
        let hits = vec![hit("m1", 1, "owner", "x"), hit("m2", 3, "owner", "x")];
        let titles = vec![title("t1", "owner", "x", 0), title("t2", "owner", "x", 2)];
        let (groups, matched) = group_hits(
            titles,
            hits,
            SearchLimits {
                sessions: 3,
                ..SearchLimits::default()
            },
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.session_id.as_str()).collect();
        assert_eq!(ids, ["t1", "m1", "t2"]);
        assert_eq!(matched, 4);
    }

    #[tokio::test]
    async fn a_full_title_window_truncates_and_scan_bounds_both_queries() {
        let titles: Vec<TitleHit> = (0..5)
            .map(|i| title(&format!("t{i}"), "owner", "x", i))
            .collect();
        let (search, _, fake) = service_with_titles(Vec::new(), titles);
        let limits = SearchLimits {
            scan: 3,
            ..SearchLimits::default()
        };
        let results = search
            .search("x", &SearchScope::default(), limits)
            .await
            .unwrap();
        assert_eq!(*fake.seen_limit.lock(), Some(3));
        assert_eq!(*fake.seen_title_limit.lock(), Some(3));
        assert!(results.truncated, "the title window filled");
        assert_eq!(results.groups.len(), 3);
        assert!(results.groups.iter().all(|g| g.title_match));
    }

    /// A title-matched group carries its title from the hit itself, so it
    /// survives a title batch that cannot load.
    #[tokio::test]
    async fn a_title_match_needs_no_title_batch() {
        let (search, sessions, _) = service_with_titles(
            vec![hit("m", 1, "owner", "x")],
            vec![title("t", "owner", "Matched title", 0)],
        );
        sessions.fail_titles(true);
        let results = search
            .search("x", &SearchScope::default(), SearchLimits::default())
            .await
            .unwrap();
        assert_eq!(results.groups[0].title.as_deref(), Some("Matched title"));
        assert!(results.groups[0].title_match);
        assert_eq!(results.groups[1].title, None);
        assert!(!results.truncated);
    }
}
