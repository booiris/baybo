//! Full-text search over chat transcript prose. See `docs/search.md`.
//!
//! FTS5 tokenizes at INSERT time with the tokenizer named in the schema, so
//! whatever the tokenizer decides is what the index can ever return. This module
//! makes no segmentation decision at all: it spaces every codepoint of a script
//! that has no word breaks, and stock `unicode61` takes it from there. A phrase
//! of character-unigrams *is* a contiguous-substring match, which is why there is
//! nothing to get wrong and no dictionary to load.

use rusqlite::OptionalExtension;
use unicode_normalization::UnicodeNormalization;

/// Identity of the index-side pipeline: `segment`'s output shape and
/// `is_unigram_script`'s ranges. Stored in `search_meta`; a mismatch on open
/// rebuilds the index from scratch. Bump it on any change to either.
///
/// Query-side work (prefix widening, a future query splitter) never reads this
/// and must never bump it — that is the whole point of keeping language handling
/// off the index side.
pub(crate) const SEGMENTER_FINGERPRINT: &str = "unigram-u61-v2";

pub(crate) const FINGERPRINT_KEY: &str = "segmenter_fingerprint";

/// Minimum Latin run length before a query chunk gets a `*`. Han is unaffected
/// either way (its tokens are single codepoints, so `库*` ≡ `库`), and prefixing
/// one or two Latin characters matches most of the corpus.
const MIN_PREFIX_LEN: usize = 3;

/// Applied at BOTH the ingest seam and the query seam — they must agree or the
/// phrase can never line up. NFD Hangul decomposes to conjoining jamo
/// (U+1100..), which is in no range below, so `segment` would no-op and the run
/// would collapse into a single token; macOS hands out NFD filenames.
pub(crate) fn normalize(text: &str) -> String {
    text.nfc().collect()
}

/// Not "ideographic" — Hangul and kana are not ideographs. The predicate is
/// "this script has no word breaks, so index it one codepoint per token".
///
/// Widening this is an INDEX-side change: bump [`SEGMENTER_FINGERPRINT`].
fn is_unigram_script(c: char) -> bool {
    matches!(c as u32,
        // 々〆〇 and friends. Without them `SESSION〇` is one token and a query
        // for `SESSION` misses.
        0x3000..=0x303F
        | 0x3040..=0x30FF      // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Ext A
        | 0x4E00..=0x9FFF      // CJK Unified
        | 0xF900..=0xFAFF      // CJK Compat
        | 0xAC00..=0xD7AF      // Hangul syllables — precomposed only; see `normalize`
        | 0xFF66..=0xFF9F      // Halfwidth Katakana — ﾃﾞｰﾀ is otherwise one token
        // CJK Ext B..H + Compat Supplement. Every codepoint in the added tail is
        // a CJK UNIFIED IDEOGRAPH and 0x323B0+ is unassigned, so there is no
        // non-CJK collateral.
        | 0x20000..=0x323AF
    )
}

/// Halfwidth voiced / semi-voiced sound marks, which modify the katakana before
/// them: `ﾃﾞ` (de) is `ﾃ` + U+FF9E, two codepoints. Unlike fullwidth kana there
/// is no precomposed form, so [`normalize`] cannot merge them — spacing them
/// apart would collide `ﾃ` (te) and `ﾃﾞ` (de) onto the same token. `unicode61`
/// classes them as L*, so they survive as part of the token they attach to.
fn is_trailing_sound_mark(c: char) -> bool {
    matches!(c as u32, 0xFF9E..=0xFF9F)
}

/// Space every unigram-script codepoint so `unicode61` emits one token each.
/// Everything else passes through untouched and keeps `unicode61`'s word
/// semantics.
pub(crate) fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev {
            // The `||` is the SCRIPT BOUNDARY, not just the Han run: `unicode61`
            // classes Han and Latin alike as L*, so without it `codex和claude`
            // is a single token and `codex` never matches.
            if (is_unigram_script(p) || is_unigram_script(c))
                && !p.is_whitespace()
                && !c.is_whitespace()
                && !is_trailing_sound_mark(c)
            {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// Wrap a segmented chunk as an FTS5 literal phrase.
///
/// Quoting is the injection boundary: `-`, `*`, `^`, `:`, `(`, `NEAR`, `OR` all
/// mean something to FTS5 bare, and doubling `"` is what keeps a hostile query
/// inside the phrase.
fn quote(segmented: &str) -> String {
    format!("\"{}\"", segmented.replace('"', "\"\""))
}

/// Does this chunk survive `unicode61`? A punctuation-only chunk tokenizes to
/// ZERO tokens, and an empty phrase is a syntax error that poisons the whole
/// expression rather than contributing nothing.
fn has_token(chunk: &str) -> bool {
    chunk.chars().any(char::is_alphanumeric)
}

/// Latin needs a prefix to reach its own suffixes — `unicode61` gives it word
/// semantics with no stemming, so `session` misses `sessions` and `compress`
/// misses `compression` entirely. Han never needs one.
fn wants_prefix(chunk: &str) -> bool {
    chunk.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= MIN_PREFIX_LEN
        && !chunk.chars().any(is_unigram_script)
}

/// Build the `MATCH` expression for a user's query string, or `None` when it
/// holds nothing indexable.
///
/// The user's own whitespace is the only split signal, and it is honoured as an
/// `AND` boundary: each chunk becomes one literal phrase.
pub(crate) fn build_match(input: &str) -> Option<String> {
    let normalized = normalize(input);
    let phrases: Vec<String> = normalized
        .split_whitespace()
        .filter(|chunk| has_token(chunk))
        .map(|chunk| {
            let segmented = segment(chunk);
            if wants_prefix(chunk) {
                format!("{}*", quote(&segmented))
            } else {
                quote(&segmented)
            }
        })
        .collect();
    (!phrases.is_empty()).then(|| phrases.join(" AND "))
}

/// The prose a row contributes to the index: its `Text` blocks, joined.
///
/// `session_messages.content` is a serialized `Vec<ContentBlock>`, so indexing
/// the column raw would index JSON keys and mime types alongside the words.
pub(crate) fn indexable_text(message: &baybo_model::ChatMessage) -> Option<String> {
    if !message.is_searchable() {
        return None;
    }
    let prose = message
        .content
        .iter()
        .filter_map(|block| match block {
            baybo_model::ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!prose.trim().is_empty()).then(|| normalize(&prose))
}

/// Mirror one row's prose into `message_fts`. Call inside the same transaction
/// as the `session_messages` insert — a row that lands in one but not the other
/// is invisible to search forever, with nothing to detect it.
pub(crate) fn index_row(
    tx: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    message: &baybo_model::ChatMessage,
) -> rusqlite::Result<()> {
    let Some(prose) = indexable_text(message) else {
        return Ok(());
    };
    index_segmented(tx, session_id, ordinal, &segment(&prose))
}

/// Insert an already-segmented row. The compaction path segments up front (it
/// cannot carry `ChatMessage` into the pool closure) but must still write the
/// row through the one statement that knows how to resolve the channel.
pub(crate) fn index_segmented(
    tx: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    segmented: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        INSERT_FTS_ROW,
        rusqlite::params![segmented, session_id, ordinal],
    )?;
    Ok(())
}

/// The channel is read back out of `sessions` in the same statement rather than
/// bound by the caller: every write path already knows the `session_id` and none
/// of them carries the `Session`, and a subquery over the primary key costs less
/// than threading the channel through three call sites. A session with no row yet
/// (only tests) indexes as the empty channel rather than failing the append.
const INSERT_FTS_ROW: &str = r#"
    INSERT INTO message_fts (segmented, session_id, ordinal, channel)
    VALUES (
        ?1, ?2, ?3,
        COALESCE((SELECT json_extract(data, '$.channel') FROM sessions WHERE id = ?2), '')
    )
"#;

/// The index's schema. Recreated, not `CREATE ... IF NOT EXISTS`-ed, so that a
/// column added here actually reaches an existing database — `IF NOT EXISTS`
/// silently keeps the old table and the next insert fails with `no such column`,
/// which surfaces as `Store::open` refusing to start.
const CREATE_FTS_TABLE: &str = r#"
    CREATE VIRTUAL TABLE message_fts USING fts5(
        segmented,
        session_id UNINDEXED,
        ordinal    UNINDEXED,
        channel    UNINDEXED,
        tokenize   = 'unicode61',
        detail     = 'full'
    )
"#;

/// Drop, recreate and refill the index whenever the pipeline that built it is
/// not the one running now. Called from `init_db`, so it completes before the
/// pool warms and `Store::open` returns: the index is never half-built, never
/// half-stale, and never a schema behind.
///
/// This is the whole migration story for `message_fts` — both its rows and its
/// columns. There is no incremental backfill and no per-row version column
/// because there is nothing to save: a full rebuild of a live corpus measures
/// ~60 ms (`segment` runs at ~350 MB/s; the FTS5 insert dominates). The absent
/// fingerprint on a fresh database performs the initial build by the same path,
/// so the `ALTER TABLE`-only migration list needs no entry.
///
/// `detail = 'full'` is load-bearing: a phrase of unigrams is the entire recall
/// mechanism, and `detail=none` rejects every multi-token phrase outright, even
/// at two tokens. It saves 11% and costs the feature.
///
/// The table carries its own content rather than `content=session_messages`:
/// external content joins on the content table's rowid, and `session_messages`
/// has a composite primary key with no INTEGER PRIMARY KEY, so its rowid is
/// *implicit* — and VACUUM renumbers implicit rowids, which would leave the index
/// silently pointing at the wrong rows. ~1 MB buys the failure mode away.
pub(crate) fn rebuild_if_stale(conn: &mut rusqlite::Connection) -> anyhow::Result<()> {
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM search_meta WHERE key = ?1",
            [FINGERPRINT_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(SEGMENTER_FINGERPRINT) {
        return Ok(());
    }

    let tx = conn.transaction()?;
    tx.execute("DROP TABLE IF EXISTS message_fts", [])?;
    tx.execute(CREATE_FTS_TABLE, [])?;
    let mut indexed = 0usize;
    {
        let mut stmt = tx.prepare(
            "SELECT session_id, ordinal, role, content, source FROM session_messages \
             ORDER BY session_id, ordinal",
        )?;
        let mut rows = stmt.query([])?;
        let mut insert = tx.prepare(INSERT_FTS_ROW)?;
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let ordinal: i64 = row.get(1)?;
            let role: String = row.get(2)?;
            let content: String = row.get(3)?;
            let source: String = row.get(4)?;
            // A row this build cannot decode is skipped, not fatal: refusing to
            // open the store over one unreadable historical row would be a far
            // worse failure than that row being unsearchable. `rehydrate_message`
            // is the sole seam that maps `(role, source)` back to the sealed
            // provenance, so the index agrees with every other read path by
            // construction. `platform_msg_id` is irrelevant to the text.
            let decoded = serde_json::from_str(&content)
                .ok()
                .and_then(|blocks| {
                    super::session::rehydrate_message(&role, blocks, &source, String::new()).ok()
                })
                .as_ref()
                .and_then(indexable_text);
            let Some(prose) = decoded else {
                continue;
            };
            insert.execute(rusqlite::params![segment(&prose), session_id, ordinal])?;
            indexed += 1;
        }
    }
    tx.execute(
        "INSERT INTO search_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![FINGERPRINT_KEY, SEGMENTER_FINGERPRINT],
    )?;
    tx.commit()?;
    tracing::info!(
        indexed,
        fingerprint = SEGMENTER_FINGERPRINT,
        "rebuilt message search index"
    );
    Ok(())
}

pub struct SqliteMessageSearchStore {
    pool: super::SqlitePool,
}

impl SqliteMessageSearchStore {
    pub fn new(pool: super::SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl baybo_store::MessageSearchStore for SqliteMessageSearchStore {
    async fn search_messages(
        &self,
        query: &str,
        scope: &baybo_store::SearchScope,
        limit: u32,
    ) -> Result<Vec<baybo_store::SearchHit>, baybo_store::StorageError> {
        let Some(expr) = build_match(query) else {
            return Ok(Vec::new());
        };
        // `None` means unrestricted. Binding NULL and testing it in SQL keeps
        // one prepared statement instead of four spellings of the same query.
        let channel = scope.channel.as_ref().map(|c| c.0.clone());
        let session = scope.session.as_ref().map(|s| s.as_str().to_owned());
        let include_hidden = scope.include_hidden;
        let include_archived = scope.include_archived;
        self.pool
            .interact("search.search_messages", move |conn| {
                // `bm25` is ascending-better, so plain ORDER BY is best-first.
                // The `session_messages` join rides its primary key.
                //
                // `channel` is a snapshot on the fts row because it is the one
                // scope axis `sessions` does not carry as a flat column (it lives
                // in the `data` blob, so joining for it means `json_extract` per
                // row). `hidden`/`archived` ARE flat and, unlike `channel`, are
                // mutable at runtime — snapshotting them would drift the moment a
                // user hides a conversation, and the fingerprint only rebuilds on
                // a segmenter change. So they are joined, never stored.
                //
                // LEFT JOIN, not JOIN: a message whose session row is missing
                // must stay findable. Session rows are never deleted (see
                // CLAUDE.md), so this cannot happen — and an INNER JOIN would
                // make it vanish silently if it ever did.
                //
                // ORDER BY / LIMIT live in the subquery so `session_messages` is
                // reached only for the rows that survive. `ORDER BY` cannot early
                // terminate — every match is scored before anything is dropped —
                // so the flat form joins and decodes the whole match set to
                // return 50 rows: measured 1.4–1.7x slower, worst on the queries
                // that match most (`的` hits 43% of the corpus). The scope
                // filters must stay INSIDE, or the limit would be applied before
                // them and return short.
                let mut stmt = conn.prepare(
                    "SELECT f.session_id, f.ordinal, f.channel, m.role, m.content, m.source, \
                            m.platform_msg_id, m.created_at, m.superseded_by, f.score \
                     FROM ( \
                         SELECT x.session_id, x.ordinal, x.channel, \
                                bm25(message_fts) AS score \
                         FROM message_fts x \
                         LEFT JOIN sessions s ON s.id = x.session_id \
                         WHERE message_fts MATCH ?1 \
                           AND (?2 IS NULL OR x.channel = ?2) \
                           AND (?3 IS NULL OR x.session_id = ?3) \
                           AND (?4 OR COALESCE(s.hidden, 0) = 0) \
                           AND (?5 OR COALESCE(s.archived, 0) = 0) \
                         ORDER BY score \
                         LIMIT ?6 \
                     ) f \
                     JOIN session_messages m \
                       ON m.session_id = f.session_id AND m.ordinal = f.ordinal \
                     ORDER BY f.score",
                )?;
                let params = rusqlite::params![
                    expr,
                    channel,
                    session,
                    include_hidden,
                    include_archived,
                    limit
                ];
                let rows = stmt.query_map(params, |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, f64>(9)?,
                    ))
                })?;
                let mut hits = Vec::new();
                for row in rows {
                    let (
                        sid,
                        ordinal,
                        chan,
                        role,
                        content,
                        source,
                        platform_msg_id,
                        created_at,
                        sup,
                        score,
                    ) = row?;
                    let blocks = serde_json::from_str(&content)
                        .map_err(|e| anyhow::anyhow!("deserialize message content: {e}"))?;
                    let message =
                        super::session::rehydrate_message(&role, blocks, &source, platform_msg_id)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    hits.push(baybo_store::SearchHit {
                        session_id: baybo_model::SessionId::from(sid),
                        ordinal,
                        channel: baybo_model::ChannelType(chan),
                        created_at: super::time::from_us(created_at).ok_or_else(|| {
                            anyhow::anyhow!(
                                "session_messages.created_at out of range: {created_at}"
                            )
                        })?,
                        superseded_by: sup,
                        message,
                        score,
                    });
                }
                Ok(hits)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChatMessage, ContentBlock, SessionId};
    use baybo_store::{MessageSearchStore, SearchScope, SessionStore};

    const TEST_SESSION: &str = "search-test";

    /// A store over one session holding `messages`, appended through the real
    /// write path so the FTS mirror is exercised rather than hand-seeded.
    ///
    /// No `sessions` row: `foreign_keys` is off (the pool sets only
    /// `busy_timeout`/`journal_mode`/`synchronous`), and the search path never
    /// joins `sessions`.
    async fn search_store(messages: &[ChatMessage]) -> SqliteMessageSearchStore {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        let sid = SessionId::from(TEST_SESSION.to_string());
        for message in messages {
            sessions
                .append_session_message(&sid, message)
                .await
                .unwrap();
        }
        SqliteMessageSearchStore::new(pool)
    }

    #[test]
    fn spaces_han_runs_into_unigrams() {
        assert_eq!(segment("今天天气"), "今 天 天 气");
    }

    #[test]
    fn leaves_latin_runs_whole() {
        assert_eq!(segment("tokio::spawn"), "tokio::spawn");
        assert_eq!(segment("hello world"), "hello world");
    }

    /// The boundary space, not the Han run, is what makes `codex` findable:
    /// `unicode61` classes Han and Latin both as L*, so the whole thing is one
    /// token without it. Real terms of exactly this shape exist in live data.
    #[test]
    fn splits_at_script_boundaries() {
        assert_eq!(segment("能用codex和claude吗"), "能 用 codex 和 claude 吗");
    }

    #[test]
    fn does_not_double_space_across_existing_whitespace() {
        assert_eq!(segment("bug 在 async"), "bug 在 async");
        assert_eq!(segment("你好\n世界"), "你 好\n世 界");
    }

    #[test]
    fn covers_scripts_outside_the_basic_cjk_block() {
        // Halfwidth katakana, the iteration mark, and CJK Ext G — each of these
        // silently collapsed into one token before its range was added.
        assert_eq!(segment("ﾃﾞｰﾀ"), "ﾃﾞ ｰ ﾀ");
        assert_eq!(segment("SESSION〇"), "SESSION 〇");
        assert_eq!(segment("\u{30001}\u{30002}"), "\u{30001} \u{30002}");
    }

    /// `ﾃﾞ` is two codepoints with no precomposed form, so `normalize` cannot
    /// merge them. Spacing them apart would put `ﾃ` (te) and `ﾃﾞ` (de) on the
    /// same token — the halfwidth analogue of dropping a Thai tone mark.
    #[test]
    fn keeps_halfwidth_sound_marks_on_their_base() {
        assert_eq!(segment("ﾃﾞ"), "ﾃﾞ");
        assert_eq!(segment("ﾊﾟ"), "ﾊﾟ");
        assert_ne!(segment("ﾃﾞｰﾀ"), segment("ﾃｰﾀ"));
    }

    #[test]
    fn normalize_makes_nfd_hangul_segmentable() {
        // NFD hangul is conjoining jamo, which no range covers, so the run would
        // reach unicode61 whole and become a single token.
        let nfd = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{116E}\u{11A8}";
        assert_eq!(nfd.chars().count(), 6, "fixture must be decomposed");
        assert_eq!(segment(&normalize(nfd)), "한 국");
    }

    #[test]
    fn query_becomes_a_unigram_phrase() {
        assert_eq!(build_match("数据库").as_deref(), Some(r#""数 据 库""#));
    }

    #[test]
    fn user_whitespace_is_the_and_boundary() {
        assert_eq!(
            build_match("数据库 迁移").as_deref(),
            Some(r#""数 据 库" AND "迁 移""#)
        );
    }

    /// Every FTS5 operator must survive as a literal. Asserted against a real
    /// FTS5 table rather than against an expected string: the thing that matters
    /// is that the engine accepts the expression and treats it as text, and only
    /// the engine can say so.
    #[tokio::test]
    async fn hostile_queries_stay_literal_and_never_error() {
        let store = search_store(&[
            ChatMessage::user(vec![ContentBlock::Text("数据库的迁移".into())]),
            ChatMessage::assistant(vec![ContentBlock::Text("OR 库 数据".into())]),
        ])
        .await;

        for hostile in [
            r#"数据" OR "库"#,
            "数据* OR 库",
            "NEAR(数据 库)",
            "数据 AND NOT 库",
            "-数据",
            "a^b:c",
            r#"""""#,
            "(((",
        ] {
            let hits = store
                .search_messages(hostile, &SearchScope::default(), 10)
                .await;
            assert!(hits.is_ok(), "hostile query {hostile:?} errored: {hits:?}");
        }
    }

    /// The whole point, end to end: a two-character Chinese word is findable
    /// mid-run, which bare `unicode61` cannot do (the entire Han run is one
    /// token, so `MATCH '迁移'` misses `数据库的迁移`).
    #[tokio::test]
    async fn finds_a_chinese_word_inside_a_han_run() {
        let store = search_store(&[
            ChatMessage::user(vec![ContentBlock::Text("数据库的迁移怎么做".into())]),
            ChatMessage::assistant(vec![ContentBlock::Text("先看 schema".into())]),
        ])
        .await;

        for query in ["迁移", "数据库", "数据", "怎么做"] {
            let hits = store
                .search_messages(query, &SearchScope::default(), 10)
                .await
                .unwrap();
            assert_eq!(hits.len(), 1, "query {query:?} should hit exactly one row");
            assert_eq!(hits[0].ordinal, 0);
        }
        // A phrase of unigrams is a *contiguous* substring: 库迁 is not one.
        assert!(
            store
                .search_messages("库迁", &SearchScope::default(), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Latin keeps word semantics, and the prefix is what reaches its suffixes.
    #[tokio::test]
    async fn latin_is_word_matched_and_prefix_reaches_suffixes() {
        let store = search_store(&[
            ChatMessage::user(vec![ContentBlock::Text("list the sessions please".into())]),
            ChatMessage::assistant(vec![ContentBlock::Text("we can trust the plan".into())]),
        ])
        .await;

        // `session` reaches `sessions` only because the chunk is widened.
        assert_eq!(
            store
                .search_messages("session", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        // `rust` must not reach `trust`: a prefix is not an infix.
        assert!(
            store
                .search_messages("rust", &SearchScope::default(), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Injected framing never reaches the index; a cron fire's own prompt does.
    #[tokio::test]
    async fn injected_framing_is_not_indexed_but_cron_runs_are() {
        let store = search_store(&[
            ChatMessage::system(vec![ContentBlock::Text("你是一个助手".into())]),
            ChatMessage::agent_context(vec![ContentBlock::Text("系统提醒".into())]),
            ChatMessage::recalled_memory(vec![ContentBlock::Text("回忆内容".into())]),
            ChatMessage::cron_fire(vec![ContentBlock::Text("定时任务".into())]),
            ChatMessage::cron_notification(vec![ContentBlock::Text("任务完成通知".into())]),
            ChatMessage::user(vec![ContentBlock::Text("真实提问".into())]),
        ])
        .await;

        for hidden in ["助手", "系统提醒", "回忆内容"] {
            assert!(
                store
                    .search_messages(hidden, &SearchScope::default(), 10)
                    .await
                    .unwrap()
                    .is_empty(),
                "{hidden:?} should not be indexed"
            );
        }
        for present in ["真实提问", "定时任务", "任务完成通知"] {
            assert_eq!(
                store
                    .search_messages(present, &SearchScope::default(), 10)
                    .await
                    .unwrap()
                    .len(),
                1,
                "{present:?} should be indexed"
            );
        }
    }

    /// Every session's prose is indexed, so scope is the caller's to choose. A
    /// subagent's own run carries channel `subagent`; the conversation that
    /// spawned it carries the parent's. Searching unscoped reaches both — which
    /// is the point, and also the trap for a user-facing search box.
    fn scope(channel: &baybo_model::ChannelType) -> SearchScope {
        SearchScope {
            channel: Some(channel.clone()),
            ..SearchScope::default()
        }
    }

    #[tokio::test]
    async fn channel_scopes_search_across_sessions() {
        use baybo_model::{ChannelType, Session, SessionState, TriggerSource, User};

        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());

        let mk = |id: &str, channel: &str| {
            let sid = SessionId::from(id.to_string());
            let ch = ChannelType::from(channel);
            Session {
                id: sid.clone(),
                user: User {
                    id: "u1".into(),
                    name: None,
                    channel: ch.clone(),
                },
                channel: ch,
                created_at: chrono::Utc::now(),
                last_active: chrono::Utc::now(),
                state: SessionState::default(),
                root_session_id: sid,
                trigger: TriggerSource::User,
                lineage: None,
                hidden: false,
                pinned: false,
                archived: false,
                folder_id: None,
                title: None,
            }
        };
        for (id, channel, text) in [
            ("chat-1", "owner", "父会话里的检索问题"),
            ("sub-1", "subagent", "子任务里的检索问题"),
        ] {
            sessions.save(&mk(id, channel)).await.unwrap();
            sessions
                .append_session_message(
                    &SessionId::from(id.to_string()),
                    &ChatMessage::assistant(vec![ContentBlock::Text(text.into())]),
                )
                .await
                .unwrap();
        }
        let store = SqliteMessageSearchStore::new(pool);

        // Unscoped reaches the subagent's private run too.
        assert_eq!(
            store
                .search_messages("检索", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            2
        );

        let owner = ChannelType::from("owner");
        let hits = store
            .search_messages("检索", &scope(&owner), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id.as_str(), "chat-1");
        assert_eq!(hits[0].channel, owner);

        let sub = ChannelType::from(baybo_model::SUBAGENT_CHANNEL_TAG);
        let hits = store
            .search_messages("检索", &scope(&sub), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id.as_str(), "sub-1");
    }

    /// "Find it in *this* chat" — and the session filter composes with the
    /// others rather than overriding them.
    #[tokio::test]
    async fn session_scopes_search_to_one_conversation() {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        for (id, text) in [("a", "第一个会话里的检索"), ("b", "第二个会话里的检索")]
        {
            sessions
                .append_session_message(
                    &SessionId::from(id.to_string()),
                    &ChatMessage::user(vec![ContentBlock::Text(text.into())]),
                )
                .await
                .unwrap();
        }
        let store = SqliteMessageSearchStore::new(pool);
        let only = |id: &str| SearchScope {
            session: Some(SessionId::from(id.to_string())),
            ..SearchScope::default()
        };

        assert_eq!(
            store
                .search_messages("检索", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            2
        );
        let hits = store.search_messages("检索", &only("a"), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id.as_str(), "a");
        assert_eq!(
            store
                .search_messages("第二个", &only("a"), 10)
                .await
                .unwrap()
                .len(),
            0,
            "a term that exists only in another session must not leak in"
        );
    }

    /// `hidden` is the user saying "remove this from my list", and it is joined
    /// live rather than snapshotted precisely because it changes after the row
    /// is indexed — the fingerprint only rebuilds on a segmenter change, so a
    /// stored copy would be stale from the moment they hid the conversation.
    #[tokio::test]
    async fn hiding_a_session_removes_it_from_search_without_reindexing() {
        use baybo_model::{ChannelType, Session, SessionState, TriggerSource, User};

        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        let sid = SessionId::from(TEST_SESSION.to_string());
        let ch = ChannelType::from("owner");
        sessions
            .save(&Session {
                id: sid.clone(),
                user: User {
                    id: "u1".into(),
                    name: None,
                    channel: ch.clone(),
                },
                channel: ch,
                created_at: chrono::Utc::now(),
                last_active: chrono::Utc::now(),
                state: SessionState::default(),
                root_session_id: sid.clone(),
                trigger: TriggerSource::User,
                lineage: None,
                hidden: false,
                pinned: false,
                archived: false,
                folder_id: None,
                title: None,
            })
            .await
            .unwrap();
        sessions
            .append_session_message(
                &sid,
                &ChatMessage::user(vec![ContentBlock::Text("被隐藏的会话内容".into())]),
            )
            .await
            .unwrap();
        let store = SqliteMessageSearchStore::new(pool);
        let find = async |scope: &SearchScope| {
            store
                .search_messages("隐藏", scope, 10)
                .await
                .unwrap()
                .len()
        };

        assert_eq!(find(&SearchScope::default()).await, 1);

        sessions.set_hidden(&sid, true).await.unwrap();

        // No reindex happened — the join reads `hidden` live.
        assert_eq!(
            find(&SearchScope::default()).await,
            0,
            "a hidden session must drop out of the default scope"
        );
        assert_eq!(
            find(&SearchScope {
                include_hidden: true,
                ..SearchScope::default()
            })
            .await,
            1,
            "an explicit include_hidden must still reach it"
        );

        sessions.set_hidden(&sid, false).await.unwrap();
        assert_eq!(
            find(&SearchScope::default()).await,
            1,
            "unhiding restores it"
        );
    }

    /// A query with nothing indexable is a user typing, not an error.
    #[tokio::test]
    async fn empty_and_punctuation_queries_return_nothing() {
        let store =
            search_store(&[ChatMessage::user(vec![ContentBlock::Text("你好".into())])]).await;
        for empty in ["", "   ", "---", "!!!"] {
            assert!(
                store
                    .search_messages(empty, &SearchScope::default(), 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    /// Compaction supersedes the originals; they must stay findable, or search
    /// goes blind exactly where a session got long enough to be worth searching.
    /// The hit reports the supersede pointer so the caller can resolve it.
    #[tokio::test]
    async fn superseded_originals_stay_indexed_and_report_their_pointer() {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        let sid = SessionId::from(TEST_SESSION.to_string());
        sessions
            .append_session_message(
                &sid,
                &ChatMessage::user(vec![ContentBlock::Text("原始的问题".into())]),
            )
            .await
            .unwrap();
        sessions
            .apply_session_compaction(
                &sid,
                &[
                    ChatMessage::system(vec![ContentBlock::Text("重播的系统提示".into())]),
                    ChatMessage::assistant(vec![ContentBlock::Text("压缩后的摘要".into())]),
                ],
            )
            .await
            .unwrap();
        let store = SqliteMessageSearchStore::new(pool);

        let hits = store
            .search_messages("原始", &SearchScope::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the pre-compaction original must stay findable"
        );
        assert_eq!(hits[0].superseded_by, Some(1));

        // The reseeded system prompt compaction wrote is hidden, so it is not
        // indexed; the summary it wrote is an assistant bubble, so it is.
        assert!(
            store
                .search_messages("系统提示", &SearchScope::default(), 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search_messages("摘要", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The fingerprint is the whole staleness defence: change the segmenter and
    /// every row must be re-indexed, or half the index speaks a different
    /// alphabet than the queries and nothing says so.
    #[tokio::test]
    async fn a_changed_fingerprint_rebuilds_the_whole_index() {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        let sid = SessionId::from(TEST_SESSION.to_string());
        for text in ["第一条消息", "第二条消息"] {
            sessions
                .append_session_message(
                    &sid,
                    &ChatMessage::user(vec![ContentBlock::Text(text.into())]),
                )
                .await
                .unwrap();
        }

        // Simulate an index built by a different segmenter: wrong rows, wrong
        // stamp. A rebuild must discard all of it and re-derive from the source.
        pool.interact("test.corrupt_index", |conn| {
            conn.execute("DELETE FROM message_fts", [])?;
            conn.execute(
                "INSERT INTO message_fts (segmented, session_id, ordinal) VALUES ('garbage', 'x', 99)",
                [],
            )?;
            conn.execute(
                "UPDATE search_meta SET value = 'stale-vN' WHERE key = ?1",
                [FINGERPRINT_KEY],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let store = SqliteMessageSearchStore::new(pool.clone());
        assert!(
            store
                .search_messages("第一条", &SearchScope::default(), 10)
                .await
                .unwrap()
                .is_empty(),
            "precondition: the corrupted index cannot answer"
        );

        pool.interact("test.rebuild", rebuild_if_stale)
            .await
            .unwrap();

        assert_eq!(
            store
                .search_messages("第一条", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .search_messages("第二条", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .search_messages("garbage", &SearchScope::default(), 10)
                .await
                .unwrap()
                .is_empty(),
            "the stale index's rows must be gone, not merged with"
        );

        let stamp: String = pool
            .interact("test.read_stamp", |conn| {
                Ok(conn.query_row(
                    "SELECT value FROM search_meta WHERE key = ?1",
                    [FINGERPRINT_KEY],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(stamp, SEGMENTER_FINGERPRINT);
    }

    /// A database written by a build whose `message_fts` had different COLUMNS
    /// must open. `CREATE VIRTUAL TABLE IF NOT EXISTS` keeps the old table, so
    /// the next insert dies on `no such column` — and because the rebuild runs
    /// inside `init_db`, that surfaces as `Store::open` refusing to start, not as
    /// a degraded search. Every other test here uses a fresh in-memory database
    /// and therefore cannot see this.
    #[tokio::test]
    async fn an_index_from_an_older_schema_is_migrated_not_fatal() {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        sessions
            .append_session_message(
                &SessionId::from(TEST_SESSION.to_string()),
                &ChatMessage::user(vec![ContentBlock::Text("旧库里的消息".into())]),
            )
            .await
            .unwrap();

        // Roll the table back to a prior shape: no `channel` column.
        pool.interact("test.downgrade_schema", |conn| {
            conn.execute("DROP TABLE message_fts", [])?;
            conn.execute(
                "CREATE VIRTUAL TABLE message_fts USING fts5(\
                     segmented, session_id UNINDEXED, ordinal UNINDEXED, \
                     tokenize = 'unicode61', detail = 'full')",
                [],
            )?;
            conn.execute(
                "UPDATE search_meta SET value = 'unigram-u61-v1' WHERE key = ?1",
                [FINGERPRINT_KEY],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        pool.interact("test.rebuild", rebuild_if_stale)
            .await
            .expect("an older index schema must migrate, not fail the open");

        let hits = store_of(&pool)
            .search_messages("旧库", &SearchScope::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].channel, baybo_model::ChannelType::from(""));
    }

    /// A fresh database is stamped by `init_db`, so an unchanged binary must not
    /// re-scan every row on every open.
    #[tokio::test]
    async fn a_matching_fingerprint_is_a_no_op() {
        let pool = super::super::SqlitePool::open_in_memory().await.unwrap();
        let sessions = super::super::SqliteSessionStore::new(pool.clone());
        sessions
            .append_session_message(
                &SessionId::from(TEST_SESSION.to_string()),
                &ChatMessage::user(vec![ContentBlock::Text("你好世界".into())]),
            )
            .await
            .unwrap();

        // Would double every row if the rebuild ran without clearing, and would
        // drop rows if it ran off a stale stamp.
        pool.interact("test.rebuild", rebuild_if_stale)
            .await
            .unwrap();
        assert_eq!(
            store_of(&pool)
                .search_messages("你好", &SearchScope::default(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    fn store_of(pool: &super::super::SqlitePool) -> SqliteMessageSearchStore {
        SqliteMessageSearchStore::new(pool.clone())
    }

    #[test]
    fn latin_gets_a_prefix_and_han_does_not() {
        assert_eq!(build_match("session").as_deref(), Some(r#""session"*"#));
        assert_eq!(build_match("会话").as_deref(), Some(r#""会 话""#));
        // Too short to widen: `to*` would match half the corpus.
        assert_eq!(build_match("to").as_deref(), Some(r#""to""#));
    }

    /// An empty phrase is an FTS5 syntax error, so a chunk that tokenizes to
    /// nothing must be dropped rather than quoted.
    #[test]
    fn drops_chunks_with_no_tokens() {
        assert_eq!(build_match("---").as_deref(), None);
        assert_eq!(build_match("").as_deref(), None);
        assert_eq!(build_match("  ").as_deref(), None);
        assert_eq!(
            build_match("数据 --- 库").as_deref(),
            Some(r#""数 据" AND "库""#)
        );
    }

    /// Everything anyone composed as a turn is in — including a cron fire's
    /// prompt and its notification. Out: text nobody composed.
    #[test]
    fn indexes_every_authored_turn_and_nothing_else() {
        let text = || vec![ContentBlock::Text("你好".into())];
        assert!(indexable_text(&ChatMessage::user(text())).is_some());
        assert!(indexable_text(&ChatMessage::user_interjection(text())).is_some());
        assert!(indexable_text(&ChatMessage::assistant(text())).is_some());
        assert!(indexable_text(&ChatMessage::cron_notification(text())).is_some());
        assert!(indexable_text(&ChatMessage::cron_fire(text())).is_some());

        // The memory backend already owns this prose; indexing stores it twice.
        assert!(indexable_text(&ChatMessage::recalled_memory(text())).is_none());
        // Skill reminders, <system-reminder> blocks, the hidden subagent
        // notification prompt — injected framing, authored by nobody.
        assert!(indexable_text(&ChatMessage::agent_context(text())).is_none());
        assert!(indexable_text(&ChatMessage::system(text())).is_none());
        assert!(indexable_text(&ChatMessage::tool(text())).is_none());
    }

    #[test]
    fn skips_rows_with_no_prose() {
        assert!(indexable_text(&ChatMessage::user(vec![])).is_none());
        assert!(
            indexable_text(&ChatMessage::user(vec![ContentBlock::Text("  ".into())])).is_none()
        );
    }
}
