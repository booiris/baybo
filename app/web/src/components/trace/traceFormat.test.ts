import { describe, expect, it } from 'vitest';
import type {
  ChatMessage,
  LlmCallResult,
  ReplayStep,
  Span,
  Step,
  StepKind,
  TurnTrace,
} from '../../types/trace';
import { compressionTokens, stepSummaryText, turnInputText, turnOutputText } from './traceFormat';

const T0 = '2026-01-01T00:00:00.000Z';
const T1 = '2026-01-01T00:00:01.000Z';

function llmSpan(result: LlmCallResult): Span {
  return {
    id: 'span-1',
    step_id: 'step-1',
    kind: {
      kind: 'llm_call',
      begin: { model_id: 'm', provider: 'p', provider_config_hash: 'h', input_messages: [] },
      result,
    },
    parallel_group: null,
    started_at: T0,
    ended_at: T1,
    outcome: { outcome: 'ok' },
  };
}

function step(kind: StepKind): Step {
  return {
    id: 'step-1',
    turn_id: 'turn-1',
    kind,
    started_at: T0,
    ended_at: T1,
    outcome: { outcome: 'ok' },
  };
}

describe('compressionTokens', () => {
  it('does not re-add the cache buckets already inside input_tokens', () => {
    // `input_tokens` is the whole prompt on every provider — the cache figures
    // are a billing-tier breakdown of it, not extra context beside it. Adding
    // them back inflated the compacted window by the entire cache hit.
    expect(
      compressionTokens({
        input_tokens: 12_400,
        cached_input_tokens: 7_440,
        cache_creation_input_tokens: 1_240,
        output_tokens: 260,
      }),
    ).toEqual({ input: 12_400, output: 260 });
  });

  it('treats missing token fields as zero', () => {
    expect(compressionTokens({})).toEqual({ input: 0, output: 0 });
  });
});

describe('stepSummaryText — compression', () => {
  const result: LlmCallResult = {
    input_tokens: 12_400,
    cached_input_tokens: 7_440,
    cache_creation_input_tokens: 1_240,
    output_tokens: 260,
  };

  it('leads with why it ran', () => {
    expect(
      stepSummaryText(step({ kind: 'compression', trigger: 'threshold' }), [llmSpan(result)]),
    ).toBe('threshold · 12,400 → 260 tokens');
    expect(
      stepSummaryText(step({ kind: 'compression', trigger: 'forced' }), [llmSpan(result)]),
    ).toBe('forced · 12,400 → 260 tokens');
  });

  it('still names a compaction whose summarizer call left no result', () => {
    // A failed summarizer call has no token figures to report, but the row
    // still has to say a compaction was attempted here.
    expect(stepSummaryText(step({ kind: 'compression', trigger: 'threshold' }), [])).toBe(
      'threshold',
    );
  });

  it('omits the trigger on a legacy row that never recorded one', () => {
    expect(stepSummaryText(step({ kind: 'compression' }), [llmSpan(result)])).toBe(
      '12,400 → 260 tokens',
    );
  });

  it('falls back only when a legacy row says nothing at all', () => {
    expect(stepSummaryText(step({ kind: 'compression' }), [])).toBe('compression');
  });
});

describe('turnInputText / turnOutputText — meta steps riding the turn', () => {
  const TITLE_PROMPT =
    'You are titling a brand-new conversation from the user\'s first message.\n\nUser\'s first message:\n<user_message>\nwhat broke the build?\n</user_message>';

  function userMsg(text: string): ChatMessage {
    return { role: 'user', content: [{ Text: text }], source: 'user' };
  }

  function inlineLlmSpan(id: string, messages: ChatMessage[], output: string): Span {
    return {
      id,
      step_id: `step-${id}`,
      kind: {
        kind: 'llm_call',
        begin: { model_id: 'm', provider: 'p', provider_config_hash: 'h', input_messages: messages },
        result: { output_content: output },
      },
      parallel_group: null,
      started_at: T0,
      ended_at: T1,
      outcome: { outcome: 'ok' },
    };
  }

  function replayStep(id: string, kind: StepKind, spans: Span[]): ReplayStep {
    return {
      step: { id, turn_id: 'turn-1', kind, started_at: T0, ended_at: T1, outcome: { outcome: 'ok' } },
      spans,
    };
  }

  function trace(steps: ReplayStep[]): TurnTrace {
    return { turn_id: 'turn-1', session_id: 'sess-1', turn_status_kind: 'completed', turn_input_kind: 'user_chat', steps };
  }

  // The title pass is spawned before the turn's first iteration, so its step
  // sorts first — reading "the turn's first LLM span" showed the prompt template
  // in the turn list and the turn-summary panel instead of the actual question.
  it('skips a leading title_generation step and reads the real question', () => {
    const t = trace([
      replayStep('s0', { kind: 'title_generation' }, [
        inlineLlmSpan('a', [userMsg(TITLE_PROMPT)], 'Build failure triage'),
      ]),
      replayStep('s1', { kind: 'llm_iteration' }, [
        inlineLlmSpan('b', [userMsg('what broke the build?')], 'The lint job failed.'),
      ]),
    ]);
    expect(turnInputText(t, [])).toBe('what broke the build?');
    expect(turnOutputText(t)).toBe('The lint job failed.');
  });

  // The observer and a compaction land AFTER the reply, so the trailing-span
  // read surfaced a progress notice / summary as the turn's answer.
  it('skips trailing progress_observer and compression steps for the output', () => {
    const t = trace([
      replayStep('s0', { kind: 'llm_iteration' }, [
        inlineLlmSpan('a', [userMsg('what broke the build?')], 'The lint job failed.'),
      ]),
      replayStep('s1', { kind: 'progress_observer' }, [
        inlineLlmSpan('b', [userMsg('summarize progress')], 'Still checking CI…'),
      ]),
      replayStep('s2', { kind: 'compression', trigger: 'threshold' }, [
        inlineLlmSpan('c', [userMsg('summarize the transcript')], 'Earlier: CI triage.'),
      ]),
    ]);
    expect(turnInputText(t, [])).toBe('what broke the build?');
    expect(turnOutputText(t)).toBe('The lint job failed.');
  });

  // A standalone `/compact` has no agent-loop step at all — its compaction is
  // the only thing the turn did, so it still gets shown rather than blanked.
  it('falls back to the turn own work when there is no llm_iteration', () => {
    const t = trace([
      replayStep('s0', { kind: 'compression', trigger: 'forced' }, [
        inlineLlmSpan('a', [userMsg('summarize the transcript')], 'Earlier: CI triage.'),
      ]),
    ]);
    expect(turnInputText(t, [])).toBe('summarize the transcript');
    expect(turnOutputText(t)).toBe('Earlier: CI triage.');
  });

  // A turn that died before recording an iteration leaves the title step alone
  // in the turn. The fallback must NOT reach for it — that is the template again.
  it('reports nothing rather than falling back to a lone side pass', () => {
    const t = trace([
      replayStep('s0', { kind: 'title_generation' }, [
        inlineLlmSpan('a', [userMsg(TITLE_PROMPT)], 'Build failure triage'),
      ]),
    ]);
    expect(turnInputText(t, [])).toBeNull();
    expect(turnOutputText(t)).toBeNull();
  });
});
