import { describe, expect, it, vi } from 'vitest';
import { InstallPrompt, type InstallPromptEvent } from './install';

function fakeEvent(): InstallPromptEvent & { prompt: ReturnType<typeof vi.fn> } {
  return {
    preventDefault: vi.fn(),
    prompt: vi.fn(() => Promise.resolve(undefined)),
  };
}

describe('InstallPrompt', () => {
  it('offers the button only once a browser hands over an event', () => {
    const prompt = new InstallPrompt();
    expect(prompt.getSnapshot().canInstall).toBe(false);

    prompt.offer(fakeEvent());

    expect(prompt.getSnapshot().canInstall).toBe(true);
  });

  it('keeps the snapshot identity across repeat offers', () => {
    const prompt = new InstallPrompt();
    const notify = vi.fn();
    prompt.subscribe(notify);

    prompt.offer(fakeEvent());
    const first = prompt.getSnapshot();
    prompt.offer(fakeEvent());

    expect(prompt.getSnapshot()).toBe(first);
    expect(notify).toHaveBeenCalledTimes(1);
  });

  it('spends the deferred prompt exactly once', async () => {
    const prompt = new InstallPrompt();
    const event = fakeEvent();
    prompt.offer(event);

    await prompt.prompt();
    // A second `prompt()` on the same event rejects in Chrome, so the button
    // must be gone and the call must not reach the event again.
    await prompt.prompt();

    expect(event.prompt).toHaveBeenCalledTimes(1);
    expect(prompt.getSnapshot().canInstall).toBe(false);
  });

  it('ignores a prompt with nothing deferred', async () => {
    const prompt = new InstallPrompt();
    await expect(prompt.prompt()).resolves.toBeUndefined();
  });

  it('withdraws the button once the app is installed', () => {
    const prompt = new InstallPrompt();
    prompt.offer(fakeEvent());

    prompt.onInstalled();

    expect(prompt.getSnapshot().canInstall).toBe(false);
  });
});
