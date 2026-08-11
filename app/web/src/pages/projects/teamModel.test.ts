import { describe, expect, it } from 'vitest';

import type { Agent, IssueRun } from './boardModel';
import { handleOf, handleProblem, workingAgentIds } from './teamModel';

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

describe('workingAgentIds', () => {
  it('counts running work and not queued work', () => {
    const working = workingAgentIds([run('a', 'running'), run('b', 'queued')]);
    expect([...working]).toEqual(['a']);
  });

  it('is empty when nothing is in flight', () => {
    expect(workingAgentIds([]).size).toBe(0);
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
