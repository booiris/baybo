import { describe, it, expect } from 'vitest';

import { isGoalCommand, previewForUserText } from './ChatPage';

// The sidebar preview strips the `/goal` prefix so a started goal reads as its
// objective (matching the `User` row the agent persists), and the control-only
// subcommands never become a preview (they leave no user row server-side).

describe('previewForUserText', () => {
  it('strips the /goal prefix and keeps the objective (casing intact)', () => {
    expect(previewForUserText('/goal climb the mountain')).toBe('climb the mountain');
    expect(previewForUserText('/Goal Ship The Parser')).toBe('Ship The Parser');
    expect(previewForUserText('  /goal   ship it  ')).toBe('ship it');
  });

  it('tolerates a Telegram @bot suffix on the command token', () => {
    expect(previewForUserText('/goal@MyBot ship it')).toBe('ship it');
  });

  it('returns null for bare /goal and the control subcommands (not user turns)', () => {
    expect(previewForUserText('/goal')).toBeNull();
    expect(previewForUserText('/goal pause')).toBeNull();
    expect(previewForUserText('/goal RESUME')).toBeNull();
    expect(previewForUserText('/goal@Bot clear')).toBeNull();
  });

  it('treats a subcommand word followed by more text as an objective', () => {
    expect(previewForUserText('/goal pause the deployment')).toBe('pause the deployment');
  });

  it('passes non-/goal text through unchanged, including look-alikes', () => {
    expect(previewForUserText('just a normal message')).toBe('just a normal message');
    expect(previewForUserText('/goalkeeper duties')).toBe('/goalkeeper duties');
    expect(previewForUserText('/compact')).toBe('/compact');
  });
});

// Drives the goal-banner prompt refetch: any `/goal …` form (set OR a control
// subcommand) mutates the goal, so all of them must trigger; look-alikes must not.
describe('isGoalCommand', () => {
  it('matches every /goal form (set + control subcommands)', () => {
    expect(isGoalCommand('/goal climb the mountain')).toBe(true);
    expect(isGoalCommand('/goal')).toBe(true);
    expect(isGoalCommand('/goal pause')).toBe(true);
    expect(isGoalCommand('/GOAL@MyBot resume')).toBe(true);
    expect(isGoalCommand('  /goal clear  ')).toBe(true);
  });

  it('rejects look-alikes and other commands', () => {
    expect(isGoalCommand('/goalkeeper duties')).toBe(false);
    expect(isGoalCommand('/compact')).toBe(false);
    expect(isGoalCommand('tell me about my goal')).toBe(false);
    expect(isGoalCommand('')).toBe(false);
  });
});
