import { describe, expect, it } from 'vitest';

import { botttsFace } from './botttsFace';

const DEV_1 = '01KZAD1QBS4A1XH456XJ7AC0V9';
const DEV_2 = '01KZ85BA2R7HZ6ZNY4N6RSV52N';

describe('botttsFace', () => {
  it('draws an inline SVG rather than pointing at dicebear.com', () => {
    const face = botttsFace(DEV_1);
    expect(face.startsWith('data:image/svg+xml')).toBe(true);
    expect(face).not.toContain('dicebear.com/9.x');
  });

  it('gives the same agent the same face, and two agents different ones', () => {
    expect(botttsFace(DEV_1)).toBe(botttsFace(DEV_1));
    expect(botttsFace(DEV_1)).not.toBe(botttsFace(DEV_2));
  });

  it('places the robot on the board’s own tints, not dicebear’s palette', () => {
    // Every seed lands on one of the four warm faces the board already uses.
    const tints = ['aecbdd', 'd9bfd4', 'cdd4ab', 'e5c9a0'];
    for (const seed of [DEV_1, DEV_2, 'lead', 'reviewer', 'qa']) {
      const svg = decodeURIComponent(botttsFace(seed).replace(/^data:image\/svg\+xml;utf8,/, ''));
      expect(tints.some((tint) => svg.includes(`#${tint}`)), seed).toBe(true);
    }
  });

  it('keeps the credit the artwork travels with', () => {
    // The only place Pablo Stanley is named is the SVG's own metadata
    // block. Anything that trims the output to save bytes has to keep it.
    const svg = decodeURIComponent(botttsFace(DEV_1));
    expect(svg).toContain('Pablo Stanley');
    expect(svg).toContain('bottts.com');
  });
});
