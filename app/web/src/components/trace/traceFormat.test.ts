import { describe, expect, it } from 'vitest';
import type { LlmCallResult, Span, Step, StepKind } from '../../types/trace';
import { compressionTokens, stepSummaryText } from './traceFormat';

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
    job_id: 'job-1',
    kind,
    started_at: T0,
    ended_at: T1,
    outcome: { outcome: 'ok' },
  };
}

describe('compressionTokens', () => {
  it('counts cache reads and writes as compacted context', () => {
    // A cached token still occupied the window that was compacted, so it must
    // be part of the "before" figure — counting `input_tokens` alone
    // under-reports and disagrees with the token totals shown elsewhere.
    expect(
      compressionTokens({
        input_tokens: 12_400,
        cached_input_tokens: 7_440,
        cache_creation_input_tokens: 1_240,
        output_tokens: 260,
      }),
    ).toEqual({ input: 21_080, output: 260 });
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

  it('leads with the trigger, so the row says whether the next prompt changed', () => {
    expect(stepSummaryText(step({ kind: 'compression', trigger: 'inline' }), [llmSpan(result)])).toBe(
      'inline · 21,080 → 260 tokens',
    );
    expect(stepSummaryText(step({ kind: 'compression', trigger: 'forced' }), [llmSpan(result)])).toBe(
      'forced · 21,080 → 260 tokens',
    );
    expect(
      stepSummaryText(step({ kind: 'compression', trigger: 'background' }), [llmSpan(result)]),
    ).toBe('background · 21,080 → 260 tokens');
  });

  it('omits the trigger on a legacy row that never recorded one', () => {
    expect(stepSummaryText(step({ kind: 'compression' }), [llmSpan(result)])).toBe(
      '21,080 → 260 tokens',
    );
  });

  it('falls back when the compaction has no llm span', () => {
    expect(stepSummaryText(step({ kind: 'compression', trigger: 'inline' }), [])).toBe('compression');
  });
});
