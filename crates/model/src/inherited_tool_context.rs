/// Marker that the active execution lineage is unattended (a cron fire, or
/// delegated work descended from one): it cannot raise approval prompts, so a
/// tool call with uncovered approval-gated resource accesses is denied
/// instead of parking on a prompt nobody can answer.
///
/// Deliberately separate from persistent session state: it follows the live
/// execution lineage, but a later independent turn does not reconstruct it
/// from the session's trigger. `Some(InheritedToolContext)` is therefore
/// meaningful and distinct from no inherited context.
#[derive(Debug, Clone, Default)]
pub struct InheritedToolContext;
