import { beforeEach, describe, expect, it } from 'vitest';
import { installMemoryLocalStorage } from '../../test/memoryStorage';
import {
  DEFAULT_PANEL_PREFS,
  PANEL_MIN_WIDTH_PX,
  THREAD_MIN_WIDTH_PX,
  clampPanelWidth,
  readPanelPrefs,
  writePanelPrefs,
} from './panelPrefs';

beforeEach(() => installMemoryLocalStorage());

describe('browser panel prefs', () => {
  it('round-trips open and width', () => {
    writePanelPrefs({ open: true, width: 640 });
    expect(readPanelPrefs()).toEqual({ open: true, width: 640 });
  });

  it('falls back to the defaults on missing or garbled storage', () => {
    expect(readPanelPrefs()).toEqual(DEFAULT_PANEL_PREFS);
    window.localStorage.setItem('baybo.chat.browserPanel', '{not json');
    expect(readPanelPrefs()).toEqual(DEFAULT_PANEL_PREFS);
    window.localStorage.setItem('baybo.chat.browserPanel', JSON.stringify({ open: 'yes', width: 'wide' }));
    expect(readPanelPrefs()).toEqual({ open: false, width: DEFAULT_PANEL_PREFS.width });
  });

  it('clamps width between the minimum and the max share of the container', () => {
    expect(clampPanelWidth(10, 1000)).toBe(PANEL_MIN_WIDTH_PX);
    expect(clampPanelWidth(5000, 2000)).toBe(1400);
    expect(clampPanelWidth(500, 1000)).toBe(500);
    // The thread keeps its minimum before the share cap bites.
    expect(clampPanelWidth(5000, 1000)).toBe(1000 - THREAD_MIN_WIDTH_PX);
    // A container too narrow for the max share still allows the minimum.
    expect(clampPanelWidth(500, 300)).toBe(PANEL_MIN_WIDTH_PX);
  });
});
