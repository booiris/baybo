import { runState, type Agent, type IssueRun } from './boardModel';
import type { AvatarRun } from './Avatar';

/// Where each teammate stands, over every run on the board — one face per
/// agent, so one answer per agent.
///
/// The rank is the whole idea, because an agent can be on three cards at
/// once: **working** outranks the rest, since a teammate who is actually
/// going is not idle whatever else is stacked behind them; and among the
/// waits, **held** outranks **queued**, because a queued run starts on its
/// own when a slot frees and a held one does not start until somebody raises
/// a ceiling. The dimmed face means "idle *and* something is waiting", which
/// is a different thing to see from plainly idle.
///
/// A held run used to answer to neither of the two sets this replaced, so a
/// board whose whole team the budget had stopped drew a full strip of grey
/// idle dots with nothing anywhere to say why.
const RANK: Record<Exclude<AvatarRun, null>, number> = { running: 3, held: 2, queued: 1 };

export function agentRunStates(activeRuns: IssueRun[]): Map<string, AvatarRun> {
  const states = new Map<string, AvatarRun>();
  for (const run of activeRuns) {
    const state = runState(run.status);
    if (state === null) continue;
    const seen = states.get(run.agent_id);
    if (seen == null || RANK[state] > RANK[seen]) states.set(run.agent_id, state);
  }
  return states;
}

export function handleOf(team: Agent[], agentId: string): string {
  return team.find((agent) => agent.id === agentId)?.handle ?? agentId;
}

/// One `baybo.json` LLM entry, as the board's pickers need it.
export type LlmEntry = {
  name: string;
  /// Every model this entry can be pinned to: its own `model` first, then
  /// each `model_list` id, de-duped. The server validates a pick against
  /// exactly this set.
  models: string[];
  /// The thinking rungs this entry's provider can express. **Empty is
  /// meaningful**: it says baybo sends this provider no effort at all, so
  /// offering a ladder would be offering a pick that never reaches the wire.
  efforts: string[];
};

export type ModelPool = { entries: LlmEntry[]; defaultName: string } | null;

/// An agent's LLM pin as the form holds it. `''` is "inherit" at every
/// level — the empty row of each picker — which is what the wire sends as
/// `null`.
export type LlmPinValue = { llm: string; model: string; effort: string };

export const UNPINNED: LlmPinValue = { llm: '', model: '', effort: '' };

/// Which entry a pin actually runs on: the one it names, or `default-llm`
/// when it names none. What the model and thinking rows are drawn from —
/// they describe the entry the agent *will* use, not the one it spelled out.
export function effectiveEntry(pool: ModelPool, pinned: string): LlmEntry | null {
  if (pool === null) return null;
  const name = pinned === '' ? pool.defaultName : pinned;
  return pool.entries.find((entry) => entry.name === name) ?? null;
}

/// One home for the rule that a pin the pool has never heard of stays
/// **visible and clearable** rather than vanishing: an entry dropped from
/// `baybo.json`, a model taken off a `model_list`, a rung a provider stopped
/// expressing. A row that disappears leaves the picker showing something
/// else and the agent still failing on the old value; a row that says
/// "(unavailable)" is the visible version of a pin that will not work.
function withStale(rows: { value: string; label: string }[], pinned: string) {
  if (pinned === '' || rows.some((row) => row.value === pinned)) return rows;
  return [...rows, { value: pinned, label: `${pinned} (unavailable)` }];
}

/// The entry picker's rows: the inherit row first, then one per entry by
/// name.
///
/// The inherit row is labelled with the entry it resolves to today
/// (`Default · deepseek`) but carries the **empty** value, so an agent that
/// takes it follows `default-llm` wherever that moves.
///
/// The entry which *is* default today still gets its own named row. It has
/// to: a model can only be picked within a named entry — the server refuses
/// a model with no entry — so collapsing the two, as this picker did when
/// the entry was the only level, made every model inside the deployment's
/// most-used entry unreachable.
export function llmOptions(pool: ModelPool, pinned: string): { value: string; label: string }[] {
  if (pool === null) return [];
  return withStale(
    [
      { value: '', label: `Default · ${pool.defaultName}` },
      ...pool.entries.map((entry) => ({ value: entry.name, label: entry.name })),
    ],
    pinned,
  );
}

/// The model picker's rows for whichever entry the pin resolves to. The
/// empty row follows that entry's own default model, so an entry serving one
/// model needs nothing set.
export function modelOptions(
  pool: ModelPool,
  entry: string,
  pinned: string,
): { value: string; label: string }[] {
  const resolved = effectiveEntry(pool, entry);
  if (resolved === null) return [];
  return withStale(
    [
      { value: '', label: `${resolved.models[0] ?? 'default'} (entry default)` },
      ...resolved.models.map((model) => ({ value: model, label: model })),
    ],
    pinned,
  );
}

/// The thinking picker's rows — taken from the ENTRY, never a local ladder:
/// each provider speaks its own effort vocabulary, and a rung its dialect
/// cannot say is a pick the gateway refuses. An entry with no rungs at all
/// gets no rows, and the caller draws no field.
export function effortOptions(
  pool: ModelPool,
  entry: string,
  pinned: string,
): { value: string; label: string }[] {
  const resolved = effectiveEntry(pool, entry);
  if (resolved === null || resolved.efforts.length === 0) return [];
  return withStale(
    [
      { value: '', label: 'entry default' },
      ...resolved.efforts.map((level) => ({ value: level, label: level })),
    ],
    pinned,
  );
}

/// Longest handle the grammar accepts. Mirrors `MAX_AGENT_HANDLE_CHARS`.
const MAX_HANDLE_CHARS = 32;


/// Why this name cannot be an agent's, or null when it can.
///
/// A name **is** a handle — the server's `AgentHandle::parse`, mirrored here
/// so the form can refuse before the round trip rather than after it. The
/// server is still the judge: it also settles collisions, which no client can
/// know about.
export function handleProblem(name: string): string | null {
  const value = name.trim();
  if (value === '') return null;
  if (!/^[a-z]/.test(value)) return 'has to start with a lowercase letter';
  if (!/^[a-z0-9-]*$/.test(value)) return 'lowercase letters, digits and “-” only';
  if (value.endsWith('-')) return 'cannot end with “-”';
  if (value.length > MAX_HANDLE_CHARS) return `at most ${MAX_HANDLE_CHARS} characters`;
  return null;
}
