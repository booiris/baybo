# Search — full-text search over chat transcripts

Search runs on SQLite **FTS5**, which is already compiled into the binary: `libsqlite3-sys`'s
bundled path passes `-DSQLITE_ENABLE_FTS5` unconditionally (`build.rs:133`), so the amalgamation
`rusqlite = { features = ["bundled"] }` builds carries it. No feature flag, no `bundled-full`, no
extra C, no new musl friction.

The tokenizer is stock **`unicode61`**. Nothing registers a custom FTS5 tokenizer, and nothing loads
a dictionary. All language handling happens in Rust, **before** the row reaches SQLite: `segment()`
puts a space around every codepoint of a script that has no word breaks, and `unicode61` then does
what it already does well — split on whitespace and punctuation, case-fold.

```
今天天气很好                     -> 今 天 天 气 很 好
这个 bug 在 async fn 里          -> 这 个  bug 在 async fn 里
```

A Han run becomes one token per character. Latin passes through untouched. Queries go through the
same function and are issued as **phrase** queries, so `数据库` becomes `MATCH '"数 据 库"'`.

A phrase of character-unigrams *is* a contiguous-substring match. That equivalence is the whole
design: **there is no segmentation decision, so there is nothing a segmentation decision can hide.**

> **There is no dictionary, and that is the design, not a gap.** Reaching for jieba/lindera/ICU on
> the index side is the obvious move and it makes recall strictly worse. Read
> [Why there is no dictionary](#why-there-is-no-dictionary) before proposing one again.

## Schema

```sql
CREATE VIRTUAL TABLE message_fts USING fts5(
    segmented,
    session_id UNINDEXED,
    ordinal    UNINDEXED,
    channel    UNINDEXED,
    tokenize   = 'unicode61',
    detail     = 'full'
);

CREATE TABLE IF NOT EXISTS search_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

`search_meta` goes in `init_db`'s `execute_batch`. **`message_fts` deliberately does not** — it is
dropped and recreated by `rebuild_if_stale` off the fingerprint. `CREATE VIRTUAL TABLE IF NOT EXISTS`
cannot migrate a column onto a table that already exists: it silently keeps the old shape, the next
insert dies on `no such column`, and because the rebuild runs inside `init_db`, that surfaces as
**`Store::open` refusing to start**. The fingerprint therefore owns the schema, not just the rows.

**`detail = 'full'` is load-bearing.** A phrase of unigrams is the entire recall mechanism, and
`detail=none` rejects every multi-token phrase outright — even at two tokens
(`fts5: phrase queries are not supported (detail!=full)`). It saves 11% and costs the feature.

**The table carries its own content — deliberately.** `content='session_messages'` (external
content) would tie the index to `session_messages`' rowid, and that table's primary key is the
composite `(session_id, ordinal)` with no `INTEGER PRIMARY KEY`, so its rowid is *implicit* — and
`VACUUM` renumbers implicit rowids. The index would then silently point at the wrong rows: not
"nothing found", but "someone else's message found". Storing the segmented text costs ~1 MB and
removes the failure mode entirely.

`role`, `created_at`, `superseded_by` and the original prose are joined back from `session_messages`
at query time, over its primary-key index — nothing to keep in sync, nothing to drift. In particular
compaction stamping `superseded_by` touches no FTS row.

**`channel` is the one snapshot**, resolved from `sessions` inside the insert statement. Every
session's prose is indexed — a chat, a cron fire, a subagent's own run — so scope is the caller's
choice, and `channel` is the axis to choose on. It is the same axis the chat list scopes with
(`WHERE json_extract(data,'$.channel') = 'owner'`), and it is what separates a subagent's session
(`subagent`, from `SUBAGENT_CHANNEL_TAG`) from the conversation that spawned it.

It is a snapshot for one reason: **`channel` is the only scope axis `sessions` does not carry as a
flat column** — it lives in the `data` blob, so joining for it costs a `json_extract` per row
(measured: 0.163 ms → 0.367 ms). Everything else is flat and is joined, never stored:

| axis | where | why not a column here |
|---|---|---|
| `role`, `created_at`, `superseded_by` | `session_messages` | already joined |
| `hidden`, `archived`, `pinned`, `folder_id` | `sessions`, flat | **mutable at runtime** — a snapshot drifts the moment a user hides a conversation, and the fingerprint only rebuilds on a segmenter change |
| `lineage_kind`, `trigger_kind` | `sessions`, flat | free in the same join; `lineage_kind='subagent'` is also redundant with `channel='subagent'` (33 sessions, both) |

Being a snapshot, `channel` is the one thing here that can drift: **a future migration that rewrites
`sessions.channel` must bump `SEGMENTER_FINGERPRINT`** so the rebuild re-snapshots it. The one-time
owner collapse in `init_db` is already covered — the rebuild runs after it.

## The segmenter

```rust
/// Bump on any change to `segment`'s output shape or `is_unigram_script`'s ranges.
const SEGMENTER_FINGERPRINT: &str = "unigram-u61-v1";

/// Applied at BOTH the ingest seam and the query seam. NFD Hangul decomposes to
/// conjoining jamo (U+1100..), which is in no range below, so `segment` no-ops and
/// the whole run collapses into one token: measured 0% recall for NFD Korean, and
/// macOS filenames arrive NFD. 0 of 1275 live rows change under this.
pub(crate) fn normalize(text: &str) -> String {
    text.nfc().collect()
}

/// Not "ideographic" — Hangul and kana are not ideographs. The predicate is
/// "this script has no word breaks, so index it one codepoint per token".
fn is_unigram_script(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F      // 々〆〇 — 'SESSION〇' otherwise tokenizes whole, query 'SESSION' misses
        | 0x3040..=0x30FF    // Hiragana + Katakana
        | 0x3400..=0x4DBF    // CJK Ext A
        | 0x4E00..=0x9FFF    // CJK Unified
        | 0xF900..=0xFAFF    // CJK Compat
        | 0xAC00..=0xD7AF    // Hangul syllables — precomposed only; see normalize()
        | 0xFF66..=0xFF9F    // Halfwidth Katakana — ﾃﾞｰﾀ otherwise tokenizes whole
        | 0x20000..=0x323AF  // CJK Ext B..H + Compat Suppl. All 9,131 added codepoints
                             // are CJK UNIFIED IDEOGRAPH; 0x323B0+ is unassigned.
    )
}

/// `ﾃﾞ` (de) is `ﾃ` + U+FF9E — two codepoints with no precomposed form, so
/// `normalize` cannot merge them. Spacing them apart would land `ﾃ` (te) and
/// `ﾃﾞ` (de) on the same token: the halfwidth analogue of dropping a Thai tone
/// mark. Fullwidth kana needs no such rule — NFC composes か+゛ into が.
fn is_trailing_sound_mark(c: char) -> bool {
    matches!(c as u32, 0xFF9E..=0xFF9F)
}

fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev {
            // The `||` covers the SCRIPT BOUNDARY, not just Han runs: unicode61
            // classes Han and Latin both as L*, so 'codex和claude' is one token
            // without it and `codex` never matches.
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
```

`normalize()` must run on **both** seams or the phrase can never line up. Space overhead is +6.5%
on real prose (Han is 3 UTF-8 bytes, a space is 1, and Han is a minority of the text).

## What gets indexed

The rule lives in Rust as an exhaustive `match`, not as a SQL `WHERE` — a denylist would silently
admit the next new `MessageSource` variant.

Searchable == somebody composed it as a turn. Everything a human authored or an agent said, in every
session. `ChatMessage::is_searchable` (`model/src/message.rs`) is the one predicate:

```rust
pub fn is_searchable(&self) -> bool {
    self.from_user() || self.role == Role::Assistant || self.source == MessageSource::Cron
}
```

`from_user()` already existed and already means `User | UserInterjection` — reuse it rather than
restate it. `Role::Assistant` covers every agent turn, including the subagent-completion reply
(`append_background_completion_reply_once`, a real assistant bubble) and `CronNotification`'s
scheduled-task badge. `MessageSource::Cron` is the cron fire's own prompt. What stays out is text
nobody composed:

| excluded | rows / chars | why |
|---|---|---|
| `Role::System` | 190 / 1,182,024 | the reseeded SOUL.md, deduping to **44 distinct bodies** — one repeated 17× |
| `Role::User` + `MessageSource::Agent` | 240 / 350,103 | `<system-reminder>` blocks, skill reminders, and the *hidden* subagent-notification prompt (`append_background_notification_prompt_once`) — injected framing, typed by nobody |
| `MessageSource::RecalledMemory` | — | rides as a framed `Role::User` row; the memory backend already owns the text, so indexing stores it twice |
| `Role::Tool` | 3,947 / 18.9M | cannot be ranked beside prose — a tool row runs five orders of magnitude longer than a typed question (max 300,697 chars vs a median of 8), and `bm25` weights columns, not rows. Wants exhaustive session-scoped grep, which is a different product with a different index. |

Neither field decides alone — `MessageSource::Agent` covers both assistant output (a real turn) and
the system prompt (not one), and `Role::User` covers both a typed prompt and an injected reminder.
The predicate lives on `ChatMessage`, not in a SQL `WHERE`: a `WHERE` would silently admit the next
new `MessageSource` variant.

`session_messages.content` is `serde_json::to_string(&Vec<ContentBlock>)`, so the prose must be
projected out of `$[*].Text` — indexing the column raw would index JSON keys and mime types. Only
`Text` blocks are indexed. `ToolResult` and `Thinking` are not, and the reason is not their size:

- **`Thinking` has no referent.** `Text` records what someone said; `ToolResult` records what a
  command printed; `Thinking` records what the model considered and discarded. Surfacing a rejected
  hypothesis as if it were a decision is worse than not finding it.
- **`ToolResult` cannot be ranked next to prose.** The largest single row is 300,697 chars; a typed
  user message runs 8. `bm25` normalizes term frequency by `dl/avgdl`, and FTS5 weights *columns*,
  not rows — no `k1`/`b` makes both ends of a five-order-of-magnitude length spread rank sanely.
  Tool output wants exhaustive grep semantics scoped to a session, which is a different product
  with a different index. (A `session_id`-scoped `LIKE` runs in 6 ms today: the predicate
  short-circuits before `content` is decoded.)

**Superseded rows are indexed.** `superseded_by` marks compaction, not a user edit — the pre-
compaction originals are the real history, and the live transcript holds a reseeded system prompt
plus the compaction machinery's own re-inserted rows, which the join filters out. Skipping
them would make search go progressively blind as sessions age: the longest conversations, the ones
most worth searching, would be the emptiest. This is also what makes search useful to the agent —
it is how it recovers detail compaction threw away.

**A superseded hit is still on screen, and `superseded_by` is not a jump target.** The display read
filters `compaction_inserted = 0`, *not* `superseded_by IS NULL` (`load_active_session_messages_tail`):
"the still-present superseded originals render, the re-injected compaction copies are hidden". So a
hit's own `ordinal` is the address to navigate to. `superseded_by` names the ordinal where that
compaction's re-inserted rows begin — rows carrying `compaction_inserted = 1`, which the display read
excludes — so aiming a jump there can only ever miss. And because `apply_session_compaction` points
*every* row active at that moment at the same ordinal, in a compacted conversation most hits carry
it: labelling them "compacted, not on screen" would be both wrong and ubiquitous. What the field
actually reports is that the model's context was rewritten after this row — a fact about the LLM's
window, not about what the user can see.

## The three paths

**Write** (`storage/src/sqlite/session.rs`, all of the single / idempotent / bulk insert sites) —
project `$[*].Text`, filter on `is_searchable()`, `normalize`, `segment`, insert. **In the same
transaction as the `session_messages` insert**; otherwise one crash silently drops a row from the
index forever.

**Rebuild** — `init_db` compares `search_meta.segmenter_fingerprint` against
`SEGMENTER_FINGERPRINT`. On mismatch: `DROP TABLE IF EXISTS message_fts`, recreate, rescan, reinsert,
restamp. It runs on the `init_db` connection before `warm()` opens the pool, so `Store::open` blocks
until the index is correct — never half-built, never half-stale, never a schema behind.

There is no incremental backfill and no per-row version column, because a full rebuild of the live
94.8 MB database is **59 ms**, start to finish, inside `Store::open`. The fingerprint mechanism *is*
the migration mechanism, for rows **and columns**: on a fresh database the stamp is absent, so the
initial build takes the same path. `mod.rs`'s `ALTER TABLE`-only migration list needs no entry.

**Bump the fingerprint for any index-side change** — the segmenter, `is_unigram_script`'s ranges,
`is_searchable`, the table's columns, or a migration that rewrites `sessions.channel`. Query-side
work (prefix widening, a future query splitter) must *not* bump it; that asymmetry is the whole
reason language handling lives off the index side.

If a dictionary is ever added on the index side, the fingerprint must hash the dictionary's
**content**, not its version string — a downloaded asset labelled `v1.2` is not a promise about
bytes.

**Query** — split on whitespace, `normalize`, `segment` each chunk, wrap each in `"..."` with
internal `"` doubled, append `*` to chunks holding 3+ Latin characters, join with `AND`.

```rust
fn build_match(input: &str) -> Option<String> {
    let phrases: Vec<String> = normalize(input)
        .split_whitespace()
        .map(segment)
        // A punctuation-only chunk segments to ZERO tokens, and an empty phrase is a
        // syntax error that poisons the whole expression.
        .filter(|c| c.chars().any(char::is_alphanumeric))
        .map(|c| format!(r#""{}""#, c.replace('"', r#""""#)))
        .collect();
    (!phrases.is_empty()).then(|| phrases.join(" AND "))
}
```

Quoting is the injection boundary — `-`, `*`, `^`, `:`, `(`, `NEAR`, `OR` all mean something to
FTS5 bare. Verified: `数据" OR "库` emits `"数 据 "" OR "" 库"` and matches literally (0 rows); no
operator escapes the quotes.

Results `JOIN session_messages ON (session_id, ordinal)` for role / timestamp / prose, `LEFT JOIN
sessions` for the live scope flags, ordered by `bm25(message_fts)` ascending. `LEFT`, not inner: a
message whose session row is missing must stay findable. Session rows are never deleted (see
CLAUDE.md's "Session data is core data"), so it cannot happen — and an inner join would make it
vanish silently if it ever did.

**`snippet()` is not used** — it would return the segmented text, spaces and all. Callers get the
original prose and highlight client-side by substring, which agrees with what matched, because
phrase-of-unigrams *is* substring.

### Scope

Because every session is indexed, the caller must say which ones it means. `SearchScope`'s default
is the narrow answer — the same set the chat list renders:

```rust
pub struct SearchScope {
    pub channel: Option<ChannelType>,   // None reaches every channel, subagent runs included
    pub include_hidden: bool,           // default false
    pub include_archived: bool,
    pub include_cron_workspaces: bool,  // default false
}
```

`hidden` is the user saying *remove this from my list*, and a search box that ignores it resurfaces
exactly what they asked to lose. On the live database, one query:

```
不限 channel + 含 hidden        363   {"ios": 55, "owner": 257, "subagent": 51}
不限 channel, 排除 hidden        234   {"ios": 55, "owner": 128, "subagent": 51}
owner + 含 hidden               257   {"owner": 257}
owner, 排除 hidden  (default)   128   {"owner": 128}
```

**Half of the owner-channel hits are in sessions the user hid** (257 → 128), and a quarter of the
whole index sits in hidden sessions. That is why `hidden` is joined rather than stored: hiding a
conversation takes effect on the next query, with no reindex and no fingerprint bump.

`include_cron_workspaces` is the axis the other three do not cover, and it exists because the corpus
is wider than any client's list. A cron fire that is **not a conversation of its own** — a one-shot's
private workspace, or any fire from before recurring fires became conversations — is dropped by
`/v1/chat/sessions` (`is_private_cron_session`) and 404s on the REST attach path. Its prose is still
indexed (`MessageSource::Cron` is searchable), so without this flag search returns conversations no
client can list, and the phone can then subscribe to and even post into them — the read path
(`load_scoped_chat_session`) and the device channel's `Subscribe` both scope by channel **only**.
A *recurring* fire is a real conversation and is never affected.

The predicate mirrors `is_private_cron_session` in SQL:

```sql
AND (?6 OR NOT (COALESCE(s.trigger_kind, '') = 'cron'
     AND COALESCE(json_extract(s.data, '$.trigger.conversation'), 0) = 0))
```

`trigger_kind` is flat and is tested first so the `json_extract` is reached only by cron rows;
`conversation` lives in the `data` blob and is absent on every historical fire, which the
`COALESCE(..., 0)` reads as "not a conversation". **`COALESCE(s.trigger_kind, '')` is load-bearing:**
the join is `LEFT`, so a row whose session is missing yields `NULL` there, and a bare
`s.trigger_kind = 'cron'` makes the whole predicate `NULL` — dropping exactly the rows the `LEFT`
join exists to keep findable. `a_row_with_no_session_survives_the_cron_predicate` pins it.

This is query-side: **it does not bump `SEGMENTER_FINGERPRINT` and triggers no rebuild.**

### The prefix `*`

Chinese needs no stemming (`数据库` has no plural), and unigram phrases give it substring semantics.
English gets `unicode61`'s word semantics, which is right — searching `rust` should not match
`trust`, and on the live corpus 191 of `LIKE '%rust%'`'s 209 hits are exactly that. But word
semantics without stemming misses every suffix:

| query | truth | exact | **with `*`** |
|---|---|---|---|
| `session` | 400 | 224 | **400** |
| `compress` | 3 | **0** | **3** |
| `trace` | 208 | 33 | **208** |

English inflection is a suffix, so a prefix swallows it. On Han the `*` is structurally a no-op —
FTS5 applies it to the last token, and Han tokens are single characters, so `库*` ≡ `库`.

`tokenize='porter unicode61'` also fixes suffixes, and is rejected: it is index-side (fingerprint,
rebuild), it is English-only against a 49%-Han corpus, and it **over- and under-matches at once** —
`running` returns 254 against a truth of 181, while `session` returns 399 against 400.

## Why there is no dictionary

A dictionary on the index side decides **where tokens begin and end**. Get that decision wrong and
the token is not in the index, so no query can reach it. Measured, on the bundled 3.51.1, with
lindera 4.0.0 + UniDic:

```
处理数据  ->  处 | 理数 | 据
```

`理数` (リスウ) is a real UniDic entry, and it wins the Viterbi lattice *across* the Chinese
`处理|数据` boundary. The document then contains no token `数` and no token `理` at all:

| query | UniDic tokens | phrase | AND |
|---|---|---|---|
| 数据库 | `数 据 库` | **0** | **0** |
| 处理 | `处 理` | **0** | **0** |
| 天气 | `天 气` | **0** | **0** |

No query rewrite recovers this, and `INSERT INTO t(t) VALUES('integrity-check')` passes clean —
it validates the index against the content *using the currently registered tokenizer*, so it cannot
know the tokenizer is wrong. Silent, permanent, data-dependent, invisible to every check SQLite
offers. It also shatters short identifiers arbitrarily (`fn go io db bug mut let api url sql git`
break; `tokio async trait cargo serde http clap` survive — UniDic's `lex.csv` holds 54 single-Latin-
letter entries cheap enough to beat the unknown-word candidate, which IPADIC does not), so
`MATCH 'bug'` also returns `debug`.

Against that, unigrams measure **100% recall, 0 misses, over 19 Chinese queries** on the live
corpus, and bare `unicode61` without the spacing — the naive alternative — measures **4–29%**,
because the whole Han run is one token and `MATCH` needs whole-token equality.

The size argument is secondary but worth stating once: UniDic is **213,745,932 B** to serve a
corpus whose entire Chinese content is ~1.7 MB. jieba's dictionary is 5,071,843 B raw / 1,906,070 B
deflated, which is **+3.77%** on the 50,604,384 B musl binary — small enough that lazily downloading
it buys nothing and costs a downloader, a checksum, a host, an offline fallback, and a startup
network dependency. And lindera's own multi-language bundles route Chinese to jieba anyway
(`embed-cjk2 = [unidic, ko-dic, jieba]`), as does charabia, which registers lindera only at
`(Cj, Jpn)` and `(Hangul, Kor)`.

**A dictionary belongs on the query side, where being wrong costs a worse result instead of no
result.** See [Backlog](#backlog).

## Why the tokenizer is not inside FTS5

`fts5_tokenizer_v2` / `xCreateTokenizer_v2` *are* reachable — `libsqlite3-sys`'s
`sqlite3/bindgen_bundled_version.rs` declares them for 3.51.1, even though `rusqlite` itself has no
FTS5 surface at all. Feasibility is not the objection. Keeping the transform in Rust buys:

- **`tokenize=` never names anything we own**, so the index side has no boot dependency. A tokenizer
  that fails to register is `no such tokenizer` — a hard error at `CREATE`, and `init_db` runs
  `CREATE VIRTUAL TABLE` before `warm()` opens all 8 pooled connections. Anything the tokenizer
  needs must exist before the pool does.
- **The plain `sqlite3` CLI keeps working** against the database.
- **Symmetry is enforceable in one place.** Index and query call the same function; a custom
  tokenizer would put the query side's fate in FTS5's hands.
- **Language handling composes.** Query-side splitting, stemming, script-run routing — all are Rust
  functions over a string. `xTokenize` receives one whole column value with no room for any of it.

## Known limitations

Document these; do not code around them.

`Search is optimized for CJK and Latin. Thai, Lao, Khmer, Arabic and Hebrew are not supported.`

| script | semantics | recall | note |
|---|---|---|---|
| Chinese / Japanese / Korean (NFC) | substring | **100%** | |
| Russian / Greek | word | 90% | |
| Vietnamese | word (phrase rejoins syllables) | 100% | tone-lossy under `remove_diacritics=1` |
| English | word, no stemming | 224/400 exact → **400 with `*`** | |
| German | word | 54% | productive compounds |
| Thai / Lao / Khmer | **broken** | 6.2% / 0% / 42.9% | |
| Hindi / Burmese / Tibetan | consonant skeleton | precision 79.3% | combining marks dropped |
| Arabic / Hebrew | word, no stemming | 50% | proclitics (`كتاب` misses `الكتاب`) |

Thai is broken for the same reason unspaced Chinese would be — nothing spaces it. **It is not a
range addition.** Adding the Thai block alone yields 100% recall at **38.9% precision**: `unicode61`'s
default `categories 'L* N* Co'` excludes `Mn`, so tone marks become separators and `เขา` (he),
`เข่า` (knee) and `เข้า` (enter) collapse into one term. Fixing that needs
`categories 'L* N* Co Mn Mc'`, which is **schema-global** and takes Burmese from 100% to 0% and
Arabic from 50% to 46.4% (its word-final i'rab must split off for the stem to survive). One table,
one `categories`, two languages wanting opposite values. Real multi-language support means
per-language tables and a language router — and per-document language detection is not available to
us: 61.9% of real messages mix Han and Latin runs, and the Latin-fraction histogram over
Han-bearing messages is U-shaped, so there is no typical message to detect.

## Cost

Against the live 94.8 MB database, measured end to end through `Store::open`:

| | |
|---|---|
| all `Text` blocks | 1,839,845 B |
| after `is_searchable()` | **~308 KB** |
| **`message_fts`** | **~1 MB — about 1% of `storage.db`** |
| first open — schema + full index build | **59 ms** |
| every later open — fingerprint matches, no work | **3 ms** |
| query | 0.2 – 1.0 ms |
| the `sessions` join for the scope flags | +0.065 ms |
| **Chinese recall, 17 queries, 909 substring-truth hits** | **909 — 100%, zero misses** |

For scale, the `spans` table is 46.6 MB in the same database — some 46× the search index.

Recall is measured against substring truth over exactly the rows `is_searchable()` admits, derived
independently of the index rather than from it. It holds by construction, not by luck: every Han
codepoint is its own token at both seams, so a phrase of unigrams *is* the substring, and there is no
segmentation decision that could hide one.

`trigram` was measured as the alternative and is worse on both axes: 2.3× the index, and `MATCH`
requires 3+ characters, so two-character Chinese queries — the modal word length — return 0 and need
a `LIKE` fallback.

### How it scales

The live corpus cloned 1–100× into synthetic sessions, so token distribution stays realistic:

| | index rows | index bytes | ×source text | build | `天气` | `session` | `数据库` | `的` |
|---|---|---|---|---|---|---|---|---|
| 1× | 848 | 1.2 MB | 2.10× | 24 ms | 0.14 | 0.38 | 0.47 | 1.3 |
| 5× | 4,240 | 5.5 MB | 2.01× | 98 ms | 0.77 | 1.1 | 1.4 | 8.9 |
| 20× | 16,960 | 21.8 MB | 1.99× | 367 ms | 1.1 | 2.6 | 3.9 | 40 |
| 50× | 42,400 | 54.5 MB | 1.99× | 938 ms | 3.3 | 9.5 | 12 | 106 |
| **100×** | **84,800** | **109 MB** | **1.99×** | **1961 ms** | **5.1** | **20** | **22** | **216** |

**Unigram tokens do not blow the index up.** The postings lists really are longer than a
word-segmented index's, and it does not matter: size stays dead linear at 2.0× the source text,
because FTS5 delta-encodes the doclists as varints and a denser term is a smaller delta. This was the
open risk in the whole design, and it is measured shut.

**Query cost tracks the number of matches, not `LIMIT`.** `ORDER BY bm25(...)` cannot early
terminate — every match is scored before anything is dropped. So a realistic query stays under 25 ms
even at 100×, while `的` (one of the most common characters in the language; it hits 43% of the
corpus) reaches 216 ms. That is the shape of the ceiling: it is reached by queries that mean nothing,
not by queries anyone types. `ORDER BY rank` was measured as the escape and is **slower**, not
faster — FTS5's rank optimization does not apply here.

**Build is linear at ~20 ms/MB** and runs only on a fingerprint change, so 100× costs ~2 s once per
deploy that touches the index side, not once per boot.

## Backlog

All three are additive; none reopens a decision above.

**Query-side jieba as a zero-hit fallback.** Only the compound-with-a-gap case misses: `数据库迁移`
against text reading `数据库的迁移` is a phrase break. jieba can split the *query* where the user
did not:

```rust
let hits = self.match_rows(&phrase(q), limit).await?;
if !hits.is_empty() { return Ok(hits); }        // never touches an answered query
match split_plan(q) { Some(e) => self.match_rows(&e, limit).await, None => Ok(hits) }
```

`NEAR(…, 10)`, not `AND`. **It must be tiered.** Over 2,736 queries with zero violations, the
builders form a total order:

```
PHRASE  ⊆  NEAR3  ⊆  NEAR10  ⊆  jieba-AND  ⊆  char-AND
```

`char-AND` is `wangfenjin/simple`'s `simple_query`; our phrase baseline sits four notches tighter.
So query-side jieba can only *add* rows — it is a recall fallback, and calling it a precision
optimization is arithmetically backwards here (it is one for `simple`, whose baseline is the loosest
link). Applied unconditionally it dilutes good answers: jieba cuts `会话` into `会`+`话`, taking a
clean 56 hits to 63. Gate it on zero hits and it fires on 0 of 19 real queries. Use `hmm = false`
(HMM invents words the index cannot hold); `jieba_rs::Jieba::cut` returns `Vec<Token>`, so map
`.word`; and filter jieba's punctuation tokens — `phrase("/")` yields zero tokens and zeroes the
expression.

Cost: `jieba-rs`, +3.77% binary, **no fingerprint change, no rebuild, no schema change**. Adding,
upgrading or removing it is a deploy, not a migration. That asymmetry is the entire reason a
dictionary belongs on this side.

**A second, word-segmented column** — if ranking ever needs it. `fts5(unigrams, words, ...)`, query
`{unigrams}:"数 据 库" OR {words}:"数据库"`, rank `bm25(_, 1.0, 5.0)`. The unigram column holds the
recall floor, so a wrong dictionary costs ranking, never results. Note FTS5 has one tokenizer per
**table**, not per column, so `words` must also be produced in Rust and indexed by `unicode61`.
Measured cost: +92% index. Measured benefit on the current corpus: none — the prefix already
saturates.

**A separate grep index for tool output**, if the agent ever needs it. Different semantics
(exhaustive, session-scoped, unranked), different table.
