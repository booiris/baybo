//! Contract test: the web trace types in `app/web/src/types/trace.ts` are
//! hand-maintained (NOT codegen'd from this crate — see that file's header),
//! so they silently drift when a `StepKind` / `SpanKind` variant is added
//! here without a matching frontend update. The trace detail page maps each
//! kind to an icon/label; an unknown kind used to dereference `undefined` and
//! white-screen the whole page (PR #61's `progress_observer`). A runtime
//! fallback now degrades gracefully, but this test catches the drift at its
//! source — the moment Rust gains a tag the frontend union is missing — and
//! points at the exact file to update.

use std::path::PathBuf;

use baybo_trace::{CompressionTrigger, SpanKind, StepKind};
use baybo_turn::TurnInputKind;

/// Every hand-maintained frontend mirror of this crate's trace types.
///
/// Two viewers render these kinds from two independently maintained copies:
/// the dashboard's, and `bench/bench-web`'s standalone read-only viewer. Both
/// are checked, because a kind added to Rust and missed by *either* is a row
/// that degrades to a generic fallback in that viewer.
const MIRRORS: &[&str] = &[
    "../../app/web/src/types/trace.ts",
    "../../bench/bench-web/web/src/types/trace.ts",
];

fn mirrors() -> Vec<(&'static str, String)> {
    MIRRORS
        .iter()
        .map(|rel| {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
            let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                let p = path.display();
                panic!(
                    "cannot read the trace types at {p} ({e}); this contract test \
                     cross-checks Rust trace kinds against that file — if the viewer \
                     moved, update MIRRORS"
                )
            });
            (*rel, body)
        })
        .collect()
}

/// The dashboard mirror alone — for checks that are about the REST body shape
/// (`turn_id`, `turn_input_kind`), which `bench-web` reads from an on-disk
/// export envelope rather than from the gateway.
fn web_trace_types() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(MIRRORS[0]);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        let p = path.display();
        panic!(
            "cannot read the web trace types at {p} ({e}); this contract test \
             cross-checks Rust trace kinds against that file — if the web app \
             moved, update this path"
        )
    })
}

fn assert_kind_listed(ts: &str, ty: &str, tag: &str, mirror: &str) {
    assert!(
        ts.contains(&format!("kind: '{tag}'")),
        "{ty} `{tag}` is not listed in {mirror}. The trace detail \
         page would fall back to a generic row for it (and before that fallback \
         existed, an unknown {ty} white-screened the page — see PR #61's \
         `progress_observer`). Fix: add `| {{ kind: '{tag}' }}` to the `{ty}` \
         union there; TS then forces a matching visual-map entry in \
         TraceSessionPage.tsx."
    );
}

#[test]
fn web_trace_types_cover_every_step_kind() {
    let kinds = [
        StepKind::LlmIteration,
        StepKind::compression(CompressionTrigger::Threshold),
        StepKind::MemoryRecall,
        StepKind::MemoryWrite,
        StepKind::SkillSelection,
        StepKind::ProgressObserver,
        StepKind::TitleGeneration,
    ];
    for k in &kinds {
        // Exhaustiveness tripwire: a new `StepKind` fails to compile here until
        // added — your cue to extend `kinds` above AND the web `StepKind` union
        // (TS then forces a `STEP_VISUALS` entry). The tag itself comes from the
        // production `StepKind::tag()`, so it can't typo out of sync.
        match k {
            StepKind::LlmIteration
            | StepKind::Compression { .. }
            | StepKind::MemoryRecall
            | StepKind::MemoryWrite
            | StepKind::SkillSelection
            | StepKind::ProgressObserver
            | StepKind::TitleGeneration => {}
        }
        for (mirror, ts) in mirrors() {
            assert_kind_listed(&ts, "StepKind", k.tag(), mirror);
        }
    }
}

#[test]
fn web_trace_types_cover_every_span_kind() {
    // `SpanKind` variants carry data, so we pin their tags here rather than
    // build sample instances. The exhaustiveness tripwire below keeps this list
    // (and the frontend union) honest when a variant is added.
    for tag in ["llm_call", "tool_call"] {
        for (mirror, ts) in mirrors() {
            assert_kind_listed(&ts, "SpanKind", tag, mirror);
        }
    }
}

/// Exhaustiveness tripwire for [`SpanKind`] (never called). A new variant
/// breaks this match, your cue to extend the span tag list above and the web
/// `SpanKind` union + `SPAN_VISUALS`.
#[allow(dead_code)]
fn span_kind_exhaustiveness(k: &SpanKind) {
    match k {
        SpanKind::LlmCall { .. } | SpanKind::ToolCall { .. } => {}
    }
}

/// Every `kind: '…'` tag the TS unions declare must exist in Rust.
///
/// The two tests above guard the other direction — Rust gained a variant, the
/// frontend forgot. Nothing guarded this one, which is how `subagent` and
/// `subagent_stub` lived in the frontend union for a long time with no Rust
/// counterpart and no possible producer: the trace viewer carried render
/// branches, an icon and a legend entry for kinds that could never arrive.
#[test]
fn web_trace_types_declare_no_kinds_rust_lacks() {
    let step_tags = [
        StepKind::LlmIteration,
        StepKind::compression(CompressionTrigger::Threshold),
        StepKind::MemoryRecall,
        StepKind::MemoryWrite,
        StepKind::SkillSelection,
        StepKind::ProgressObserver,
        StepKind::TitleGeneration,
    ]
    .map(|k| k.tag());
    let span_tags = ["llm_call", "tool_call"];

    for (mirror, ts) in mirrors() {
        for (ty, known) in [
            ("StepKind", step_tags.as_slice()),
            ("SpanKind", span_tags.as_slice()),
        ] {
            for tag in declared_tags(&ts, ty) {
                assert!(
                    known.contains(&tag.as_str()),
                    "{mirror} declares `{ty}` variant `{tag}`, which no \
                     Rust variant produces. Either it was removed here and the mirror \
                     kept it, or it was never real — delete it from the union (and the \
                     viewer branches keyed on it), or add the Rust variant."
                );
            }
        }
    }
}

/// The body of one exported TS union: everything between `export type {ty} =`
/// and the blank line that ends the declaration.
///
/// The terminator is `";\n\n"`, not `";\n"`. A union member can carry its own
/// fields — `StepKind`'s `compression` declares `trigger?: … ;` on its own line
/// — and stopping at the first `;\n` cut the body off inside that member, so
/// the scrape saw two of seven tags and the contract test was quietly guarding
/// almost nothing.
fn union_body<'a>(ts: &'a str, ty: &str) -> &'a str {
    let header = format!("export type {ty} =");
    let Some(start) = ts.find(&header) else {
        panic!("app/web/src/types/trace.ts has no `{header}` — did the mirror move?")
    };
    let body = &ts[start + header.len()..];
    &body[..body.find(";\n\n").unwrap_or(body.len())]
}

/// Every occurrence of `needle` followed by a `'…'` literal, in order.
fn scrape_quoted(body: &str, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(needle) {
        rest = &rest[i + needle.len()..];
        if let Some(end) = rest.find('\'') {
            out.push(rest[..end].to_string());
            rest = &rest[end..];
        }
    }
    out
}

/// The `kind: '…'` tags inside one exported TS union.
fn declared_tags(ts: &str, ty: &str) -> Vec<String> {
    scrape_quoted(union_body(ts, ty), "kind: '")
}

/// The bare string literals of a flat union like
/// `export type CompressionTrigger = 'threshold' | 'forced';`. These have no
/// `kind: '` needle, so they need their own scrape — and they had no coverage
/// at all, which is exactly how a retired variant would linger in the mirror.
fn declared_literals(ts: &str, ty: &str) -> Vec<String> {
    // Split on the quote: the odd-indexed pieces are what sat inside it.
    union_body(ts, ty)
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The compression union, both directions. `trigger` is rendered as a raw
/// string by the trace viewer, so a drift here shows up as a row labelled with
/// a value that no longer means anything.
#[test]
fn web_trace_types_cover_the_compression_unions() {
    let ts = web_trace_types();

    let triggers = [CompressionTrigger::Threshold, CompressionTrigger::Forced];
    for t in triggers {
        // Exhaustiveness tripwire: a new variant fails to compile here.
        match t {
            CompressionTrigger::Threshold | CompressionTrigger::Forced => {}
        }
    }

    for (ty, wire) in [("CompressionTrigger", vec!["threshold", "forced"])] {
        let declared = declared_literals(&ts, ty);
        assert!(
            !declared.is_empty(),
            "scraped no literals out of `{ty}` in app/web/src/types/trace.ts — the \
             scraper is broken, not the mirror"
        );
        for tag in &wire {
            assert!(
                declared.iter().any(|d| d == tag),
                "`{ty}` variant `{tag}` is missing from app/web/src/types/trace.ts"
            );
        }
        for d in &declared {
            assert!(
                wire.contains(&d.as_str()),
                "app/web/src/types/trace.ts declares `{ty}` value `{d}`, which no Rust \
                 variant produces — delete it, or add the Rust variant"
            );
        }
    }
}

/// The turn-entity field names the trace REST bodies emit.
///
/// `crates/gateway/src/api/admin/traces.rs` builds those bodies from
/// hand-written `json!` literals against an untyped `serde_json::Value`
/// response, and the frontend mirror is hand-maintained too — so nothing
/// between the two is type-checked. A key renamed on one side and missed on
/// the other compiles, passes every other test, and renders an empty trace
/// tree. This pins both directions: the turn names must be present, and the
/// pre-rename names must be gone.
#[test]
fn web_trace_types_use_turn_field_names() {
    let ts = web_trace_types();

    for field in ["turn_id", "turn_status_kind", "turns:"] {
        assert!(
            ts.contains(field),
            "app/web/src/types/trace.ts is missing `{field}`. The trace endpoints in \
             crates/gateway/src/api/admin/traces.rs emit that key; the mirror has to \
             declare it or the trace viewer silently renders nothing."
        );
    }

    for stale in [
        "job_id",
        "job_status_kind",
        "jobs:",
        "JobTrace",
        "TraceJobSummary",
    ] {
        assert!(
            !ts.contains(stale),
            "app/web/src/types/trace.ts still carries `{stale}`. The turn entity is \
             named Turn everywhere else, so this mirror is describing a shape the \
             gateway no longer emits."
        );
    }
}

/// `TurnInputKind`, both directions — plus the chat-turn rule the viewer's
/// numbering depends on.
///
/// The viewer numbers only chat turns, so if the TS union or its `isChatTurn`
/// drifts from `TurnInputKind::is_chat_turn`, the trace sidebar silently starts
/// numbering rows the chat transcript never showed — the exact disagreement the
/// field was added to fix.
#[test]
fn web_trace_types_cover_the_turn_input_kinds() {
    let ts = web_trace_types();

    let kinds = [
        TurnInputKind::UserChat,
        TurnInputKind::Cron,
        TurnInputKind::CronNotification,
        TurnInputKind::Compact,
        TurnInputKind::Spawned,
        TurnInputKind::SubagentNotification,
    ];
    for k in kinds {
        // Exhaustiveness tripwire: a new variant fails to compile here.
        match k {
            TurnInputKind::UserChat
            | TurnInputKind::Cron
            | TurnInputKind::CronNotification
            | TurnInputKind::Compact
            | TurnInputKind::Spawned
            | TurnInputKind::SubagentNotification => {}
        }
    }

    let wire: Vec<String> = kinds
        .iter()
        .map(|k| {
            serde_json::to_value(k)
                .expect("TurnInputKind serializes")
                .as_str()
                .expect("as a string")
                .to_string()
        })
        .collect();

    let declared = declared_literals(&ts, "TurnInputKind");
    assert!(
        !declared.is_empty(),
        "scraped no literals out of `TurnInputKind` in app/web/src/types/trace.ts — \
         the scraper is broken, not the mirror"
    );
    for w in &wire {
        assert!(
            declared.contains(w),
            "`TurnInputKind` variant `{w}` is missing from app/web/src/types/trace.ts"
        );
    }
    for d in &declared {
        assert!(
            wire.contains(d),
            "app/web/src/types/trace.ts declares `TurnInputKind` value `{d}`, which no \
             Rust variant produces — delete it, or add the Rust variant"
        );
    }

    // The non-chat set is what the viewer must not number. Pin it by value, so
    // adding a non-chat kind on either side fails here rather than silently
    // renumbering the sidebar.
    let non_chat: Vec<&String> = wire
        .iter()
        .zip(kinds.iter())
        .filter(|(_, k)| !k.is_chat_turn())
        .map(|(w, _)| w)
        .collect();
    assert_eq!(
        non_chat.len(),
        2,
        "expected exactly compact + cron_notification to be non-chat, got {non_chat:?}"
    );
    for w in non_chat {
        assert!(
            ts.contains(&format!("kind !== '{w}'")),
            "app/web/src/types/trace.ts `isChatTurn` does not exclude `{w}`, so the trace \
             viewer would number a turn the chat transcript never showed"
        );
    }
}
