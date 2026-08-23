import { describe, expect, it } from 'vitest';

import type { Agent, IssueRun } from './boardModel';
import {
  agentRunStates,
  effortOptions,
  handleOf,
  handleProblem,
  llmOptions,
  modelOptions,
} from './teamModel';

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

describe('the llm pin pickers', () => {
  const pool = {
    defaultName: 'deepseek',
    entries: [
      { name: 'deepseek', models: ['deepseek-chat'], efforts: [] },
      {
        name: 'gpt-5',
        models: ['gpt-5.5', 'o3'],
        efforts: ['low', 'medium', 'high'],
      },
    ],
  };

  it('offers an inherit row that names what it resolves to, then every entry', () => {
    // The entry that is default *today* still gets its own named row: a
    // model can only be picked within a named entry, so folding it into the
    // inherit row would make every model inside the deployment's most-used
    // entry unreachable.
    expect(llmOptions(pool, '')).toEqual([
      { value: '', label: 'Default · deepseek' },
      { value: 'deepseek', label: 'deepseek' },
      { value: 'gpt-5', label: 'gpt-5' },
    ]);
  });

  it('has nothing to offer before the pool has loaded', () => {
    expect(llmOptions(null, '')).toEqual([]);
    expect(modelOptions(null, 'gpt-5', '')).toEqual([]);
    expect(effortOptions(null, 'gpt-5', '')).toEqual([]);
  });

  it('keeps a pin the pool has never heard of, marked unavailable', () => {
    // An entry dropped from baybo.json: a row that vanished would leave the
    // picker showing something else while the agent still failed on the old
    // value. Visible, and therefore clearable.
    expect(llmOptions(pool, 'retired')).toContainEqual({
      value: 'retired',
      label: 'retired (unavailable)',
    });
    expect(modelOptions(pool, 'gpt-5', 'o1')).toContainEqual({
      value: 'o1',
      label: 'o1 (unavailable)',
    });
    expect(effortOptions(pool, 'gpt-5', 'ultra')).toContainEqual({
      value: 'ultra',
      label: 'ultra (unavailable)',
    });
  });

  it('lists the models of the entry a pin resolves to', () => {
    expect(modelOptions(pool, 'gpt-5', '')).toEqual([
      { value: '', label: 'gpt-5.5 (entry default)' },
      { value: 'gpt-5.5', label: 'gpt-5.5' },
      { value: 'o3', label: 'o3' },
    ]);
  });

  it('falls back to the default entry when the pin names none', () => {
    // An unpinned agent runs on `default-llm`, so that is the entry whose
    // models and rungs the fields describe.
    expect(modelOptions(pool, '', '')).toEqual([
      { value: '', label: 'deepseek-chat (entry default)' },
      { value: 'deepseek-chat', label: 'deepseek-chat' },
    ]);
  });

  it('offers the rungs the entry can express, and none at all when it can express none', () => {
    // The ladder comes from the entry, never a local list: a rung the
    // provider's dialect cannot say is a pick that never reaches the wire.
    expect(effortOptions(pool, 'gpt-5', '')).toEqual([
      { value: '', label: 'entry default' },
      { value: 'low', label: 'low' },
      { value: 'medium', label: 'medium' },
      { value: 'high', label: 'high' },
    ]);
    expect(effortOptions(pool, 'deepseek', '')).toEqual([]);
  });
});
