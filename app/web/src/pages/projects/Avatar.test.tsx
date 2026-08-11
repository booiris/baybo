import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';

import { Avatar } from './Avatar';
import { generatedPortrait } from './portrait';

const DEV_1 = '01KZAD1QBS4A1XH456XJ7AC0V9';

describe('Avatar', () => {
  it('draws the portrait it is handed, and names the teammate around it', () => {
    render(<Avatar handle="dev-1" src={generatedPortrait(DEV_1)} />);
    const chip = screen.getByTitle('@dev-1');
    const face = chip.querySelector('img');
    expect(face?.getAttribute('src')).toBe(generatedPortrait(DEV_1));
    // The picture is decoration; the chip's title is what a screen reader
    // gets, so a second copy of the handle in the alt would only repeat it.
    expect(face?.getAttribute('alt')).toBe('');
  });

  it('keeps initials for the operator and the board, who have no face', () => {
    render(
      <>
        <Avatar handle="you" />
        <Avatar handle="board" />
      </>,
    );
    expect(screen.getByTitle('you').textContent).toBe('ME');
    expect(screen.getByTitle('@board').textContent).toBe('B');
    expect(screen.getByTitle('you').querySelector('img')).toBeNull();
  });

  it('still says what a run is doing behind the portrait', () => {
    const { container } = render(
      <Avatar handle="dev-1" src={generatedPortrait(DEV_1)} run="running" />,
    );
    expect(screen.getByTitle('@dev-1 — working')).toBeInTheDocument();
    // The spinning ring and the status dot sit outside the chip's box, so
    // clipping the portrait must not be done by clipping the chip.
    expect(container.querySelector('[class*="animate-spin"]')).not.toBeNull();
    expect(screen.getByTitle('@dev-1 — working').className).not.toContain('overflow-hidden');
  });

  it('draws no status dot for a caller that never said anything about runs', () => {
    // A comment's author, a picker's option. A grey dot on those would be
    // this component announcing "idle" on their behalf — they have no run
    // data at all, and the roster is the only place that does.
    const { container } = render(<Avatar handle="dev-1" src={generatedPortrait(DEV_1)} />);
    expect(container.querySelector('[class*="rounded-full"][class*="bg-ink-soft"]')).toBeNull();
  });

  it('draws a grey one when asked, and turns it green on a live run', () => {
    const idle = render(<Avatar handle="dev-1" src={generatedPortrait(DEV_1)} dot />);
    expect(
      idle.container.querySelector('[class*="rounded-full"][class*="bg-ink-soft"]'),
    ).not.toBeNull();

    const live = render(
      <Avatar handle="dev-2" src={generatedPortrait(DEV_1)} dot run="running" />,
    );
    expect(live.container.querySelector('[class*="bg-ok"]')).not.toBeNull();
  });
});
