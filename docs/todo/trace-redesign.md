# Trace Redesign — done, follow-ups remaining

The full design is implemented and reflected in:

- [`docs/modules/session.md`](../modules/session.md) — Session, lineage, trigger, fork rejection
- [`docs/modules/job.md`](../modules/job.md) — Job state machine (`Completed`, `Cancelled`, etc.)
- [`docs/modules/trace.md`](../modules/trace.md) — Step / Span / SpanEvent

These specs are authoritative; this file no longer carries the active design.

## Follow-ups

Tracked here so they don't get lost. Each is a scoped extension, not a redesign.

### TUI live trace stream

`TraceEventStream` flows in-process. Surfacing it across the gateway WS
protocol to the TUI for live progress display requires a new frame
variant (`Frame::TraceEvent` or similar) plus a TUI render layer. Scoped
to whichever PR adds the live-progress view.
