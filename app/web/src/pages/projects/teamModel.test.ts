import { describe, expect, it } from 'vitest';

import type { Agent, IssueRun } from './boardModel';
import { agentRunStates, handleOf, handleProblem, llmOptions, llmSelected } from './teamModel';

function run(agentId: string, status: IssueRun['status']): IssueRun {
  return {
    number: 1,
    agent_id: agentId,
    trigger: 'started',
    status,
    attempt: 1,
    created_at_ms: 0,
  };
}

function member(id: string, handle: string): Agent {
  return {
    id,
    handle,
    name: handle,
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
  };
}

describe('agentRunStates', () => {
  it('tells working apart from waiting', () => {
    const states = agentRunStates([run('a', 'running'), run('b', 'queued')]);
    expect(states.get('a')).toBe('running');
    expect(states.get('b')).toBe('queued');
  });

  it('does not draw a teammate the budget stopped as plainly idle', () => {
    // A held run answered to neither of the two sets this replaced, so a
    // board the ceiling had stopped showed a full strip of grey idle dots
    // with nothing anywhere on it to say why.
    expect(agentRunStates([run('a', 'held')]).get('a')).toBe('held');
  });

  it('ranks working over held, and held over queued', () => {
    // One face per agent, and an agent can be on three cards. Working wins
    // because a teammate who is going is not idle whatever is stacked
    // behind them; held beats queued because a queued run starts on its own
    // when a slot frees and a held one waits on somebody raising a ceiling.
    const busy = agentRunStates([run('a', 'queued'), run('a', 'held'), run('a', 'running')]);
    expect(busy.get('a')).toBe('running');
    const stopped = agentRunStates([run('a', 'queued'), run('a', 'held')]);
    expect(stopped.get('a')).toBe('held');
  });

  it('says nothing about an agent with nothing in flight', () => {
    expect(agentRunStates([]).size).toBe(0);
    expect(agentRunStates([run('a', 'running')]).get('b')).toBeUndefined();
  });
});

describe('handleOf', () => {
  it('reads the handle off the roster', () => {
    expect(handleOf([member('01J', 'dev-1')], '01J')).toBe('dev-1');
  });

  it('falls back to the id for somebody who has left', () => {
    expect(handleOf([member('01J', 'dev-1')], '01GONE')).toBe('01GONE');
  });
});

describe('handleProblem', () => {
  it('accepts what the server accepts', () => {
    expect(handleProblem('test-engineer')).toBeNull();
    expect(handleProblem('qa2')).toBeNull();
    expect(handleProblem('  robin  ')).toBeNull();
  });

  it('says nothing about an empty box, which is not yet wrong', () => {
    // The submit button is what refuses an empty name. Shouting at a field
    // the operator has not typed in is noise.
    expect(handleProblem('')).toBeNull();
  });

  it('refuses each way the grammar can be broken, in its own words', () => {
    expect(handleProblem('Test Engineer')).toMatch(/lowercase letter/);
    expect(handleProblem('42nd')).toMatch(/lowercase letter/);
    expect(handleProblem('test engineer')).toMatch(/digits and/);
    expect(handleProblem('qa/2')).toMatch(/digits and/);
    expect(handleProblem('dev-')).toMatch(/end with/);
    expect(handleProblem('d'.repeat(33))).toMatch(/32 characters/);
  });
});

describe('llmOptions / llmSelected', () => {
  const pool = { names: ['deepseek', 'gpt-5'], defaultName: 'deepseek' };

  it('lists each model once, the default one carrying the empty value', () => {
    // Not a "default" row beside the model it resolves to. Picking the
    // default's row pins nothing, so the agent follows it wherever it moves.
    expect(llmOptions(pool)).toEqual([
      { value: '', label: 'deepseek' },
      { value: 'gpt-5', label: 'gpt-5' },
    ]);
  });

  it('has nothing to offer before the pool has loaded', () => {
    expect(llmOptions(null)).toEqual([]);
  });

  it('shows an agent pinned to the current default on that model’s row', () => {
    // The only row it could show as, now that there is one per model — and
    // the row means the same thing to look at either way.
    expect(llmSelected('deepseek', pool)).toBe('');
    expect(llmSelected(null, pool)).toBe('');
    expect(llmSelected('gpt-5', pool)).toBe('gpt-5');
  });

  it('keeps a pin the pool has never heard of rather than silently dropping it', () => {
    // A model removed from baybo.json: the select shows no matching row, which
    // is the visible version of a pin that will fail when it is woken.
    expect(llmSelected('retired-model', pool)).toBe('retired-model');
  });
});
