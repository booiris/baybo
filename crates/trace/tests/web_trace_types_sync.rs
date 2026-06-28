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

use baybo_trace::{SpanKind, StepKind};

/// The hand-maintained frontend mirror of this crate's trace types.
fn web_trace_types() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/web/src/types/trace.ts");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        let p = path.display();
        panic!(
            "cannot read the web trace types at {p} ({e}); this contract test \
             cross-checks Rust trace kinds against that file — if the web app \
             moved, update this path"
        )
    })
}

fn assert_kind_listed(ts: &str, ty: &str, tag: &str) {
    assert!(
        ts.contains(&format!("kind: '{tag}'")),
        "{ty} `{tag}` is not listed in app/web/src/types/trace.ts. The trace detail \
         page would fall back to a generic row for it (and before that fallback \
         existed, an unknown {ty} white-screened the page — see PR #61's \
         `progress_observer`). Fix: add `| {{ kind: '{tag}' }}` to the `{ty}` \
         union there; TS then forces a matching visual-map entry in \
         TraceSessionPage.tsx."
    );
}

#[test]
fn web_trace_types_cover_every_step_kind() {
    let ts = web_trace_types();
    let kinds = [
        StepKind::LlmIteration,
        StepKind::Compression,
        StepKind::MemoryRecall,
        StepKind::MemoryWrite,
        StepKind::SkillSelection,
        StepKind::ProgressObserver,
    ];
    for k in &kinds {
        // Exhaustiveness tripwire: a new `StepKind` fails to compile here until
        // added — your cue to extend `kinds` above AND the web `StepKind` union
        // (TS then forces a `STEP_VISUALS` entry). The tag itself comes from the
        // production `StepKind::tag()`, so it can't typo out of sync.
        match k {
            StepKind::LlmIteration
            | StepKind::Compression
            | StepKind::MemoryRecall
            | StepKind::MemoryWrite
            | StepKind::SkillSelection
            | StepKind::ProgressObserver => {}
        }
        assert_kind_listed(&ts, "StepKind", k.tag());
    }
}

#[test]
fn web_trace_types_cover_every_span_kind() {
    let ts = web_trace_types();
    // `SpanKind` variants carry data, so we pin their tags here rather than
    // build sample instances. The exhaustiveness tripwire below keeps this list
    // (and the frontend union) honest when a variant is added.
    for tag in ["llm_call", "tool_call"] {
        assert_kind_listed(&ts, "SpanKind", tag);
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
