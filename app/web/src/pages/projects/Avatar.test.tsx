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
});
