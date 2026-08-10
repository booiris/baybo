import { describe, expect, it } from 'vitest';

import type { Agent, IssueRun } from './boardModel';
import { handleOf, previewHandle, workingAgentIds } from './teamModel';

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

describe('previewHandle', () => {
  it('slugifies a display name the way the server will', () => {
    expect(previewHandle('Test Engineer')).toBe('test-engineer');
    expect(previewHandle('Robin')).toBe('robin');
    expect(previewHandle('  Dev   One  ')).toBe('dev-one');
    expect(previewHandle('QA/2')).toBe('qa-2');
  });

  it('has no answer where the grammar has none', () => {
    // The server refuses these too; showing a guess that cannot become a
    // handle is worse than showing the plain rule instead.
    expect(previewHandle('')).toBeNull();
    expect(previewHandle('!!!')).toBeNull();
    expect(previewHandle('42nd')).toBeNull();
  });
});
