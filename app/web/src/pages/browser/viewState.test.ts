import { describe, expect, it } from 'vitest';
import type { BrowserPhase, LinkMsg, StreamState, ViewerDown } from '../../api/browserViewTypes';
import {
  INITIAL_VIEW_STATE,
  applyConnStatus,
  applyViewerDown,
  describeView,
  followedTarget,
  isInsecureRemote,
  type ViewState,
} from './viewState';
import { splitUrl } from './UrlBar';
import { isBrowserTool } from './route';

const LINK: LinkMsg = { type: 'link', up: true, link_epoch: 1, unavailable: null, ping_interval_ms: 15_000 };
const TARGET = { target_id: 'T1', url: 'https://example.com/a?b', title: 'Example' };

function fold(...msgs: ViewerDown[]): ViewState {
  return msgs.reduce(applyViewerDown, applyConnStatus(INITIAL_VIEW_STATE, { state: 'connected' }));
}

function ready(stream: StreamState): ViewState {
  return fold(
    LINK,
    { type: 'status', mode: 'host', phase: { type: 'ready' }, browser_gen: 3 },
    { type: 'stream', state: stream, browser_gen: 3, target: TARGET },
  );
}

function withPhase(phase: BrowserPhase): ViewState {
  return fold(LINK, { type: 'status', mode: 'docker', phase, browser_gen: 0 });
}

const title = (s: ViewState) => describeView(s, null)?.title;

describe('describeView', () => {
  it('reports the connection before anything the link said', () => {
    expect(title({ ...ready('live'), conn: { state: 'hidden' } })).toBe('Paused');
    expect(title(applyConnStatus(ready('live'), { state: 'connecting' }))).toBe('Connecting to the gateway…');
    const down = applyConnStatus(ready('live'), { state: 'disconnected', retryInMs: 4000 });
    expect(describeView(down, null)).toMatchObject({
      tone: 'warn',
      title: 'Disconnected from the gateway',
      detail: 'Retrying in 4s.',
    });
  });

  it('maps link availability', () => {
    expect(title(fold())).toBe('Waiting for the gateway…');
    expect(title(fold({ ...LINK, up: false }))).toBe('Browser sidecar not connected');
    expect(title(fold({ ...LINK, up: false, unavailable: 'browser_disabled' }))).toBe('Browser is disabled');
    expect(describeView(fold({ ...LINK, unavailable: 'cddm_version_mismatch' }), null)).toMatchObject({
      tone: 'err',
      title: 'Live view unavailable',
      detail: expect.stringContaining('chrome-devtools-mcp') as string,
    });
    expect(describeView(fold({ ...LINK, unavailable: 'tap_not_fired' }), null)).toMatchObject({
      tone: 'warn',
      detail: expect.stringContaining('could not hook') as string,
    });
  });

  it('maps browser phases', () => {
    expect(title(withPhase({ type: 'idle' }))).toBe('Browser not launched yet');
    expect(describeView(withPhase({ type: 'installing', percent: 42.4 }), null)).toMatchObject({
      title: 'Installing the browser…',
      detail: '42%',
      progress: 42,
    });
    expect(describeView(withPhase({ type: 'installing', percent: 140 }), null)?.progress).toBe(100);
    expect(describeView(withPhase({ type: 'docker', phase: 'docker-building-image' }), null)).toMatchObject({
      title: 'Starting the Docker browser',
      detail: 'Building the browser image…',
    });
    expect(title(withPhase({ type: 'recovering' }))).toBe('Browser recovering…');
    expect(describeView(withPhase({ type: 'failed', error: 'boom' }), null)).toMatchObject({
      tone: 'err',
      title: 'Browser failed',
      detail: 'boom',
    });
  });

  it('maps stream states', () => {
    expect(title(ready('idle'))).toBe('Browser not launched yet');
    expect(title(ready('paused'))).toBe('Stream paused');
    expect(title(ready('background'))).toBe('Tab in background');
    expect(title(ready('target_gone'))).toBe('Followed tab closed');
    expect(title(ready('unavailable'))).toBe('Live view unavailable');
  });

  it('is live only once a frame of the current target and generation is shown', () => {
    const live = ready('live');
    expect(title(live)).toBe('Waiting for the first frame…');
    expect(describeView(live, { target_id: 'T1', browser_gen: 2 })?.title).toBe('Waiting for the first frame…');
    expect(describeView(live, { target_id: 'T0', browser_gen: 3 })?.title).toBe('Waiting for the first frame…');
    expect(describeView(live, { target_id: 'T1', browser_gen: 3 })).toBeNull();
  });
});

describe('applyViewerDown', () => {
  it('forgets browser state when the link epoch changes or the link drops', () => {
    const s = ready('live');
    expect(applyViewerDown(s, LINK).stream).not.toBeNull();
    expect(applyViewerDown(s, { ...LINK, link_epoch: 2 }).stream).toBeNull();
    expect(applyViewerDown(s, { ...LINK, up: false }).browser).toBeNull();
  });

  it('keeps an error across a reconnect attempt and clears it once connected', () => {
    const s = applyViewerDown(ready('live'), { type: 'error', code: 'too_many_viewers' });
    const down = applyConnStatus(s, { state: 'disconnected', retryInMs: 1000 });
    expect(describeView(down, null)?.detail).toContain('Too many viewers');
    expect(applyConnStatus(down, { state: 'connected' }).error).toBeNull();
  });
});

describe('followedTarget', () => {
  it('prefers the stream target, then the followed tab', () => {
    expect(followedTarget(ready('live'))).toEqual(TARGET);
    const other = { target_id: 'T2', url: 'about:blank', title: '' };
    const s = fold(LINK, { type: 'targets', browser_gen: 0, targets: [TARGET, other], followed: 'T2' });
    expect(followedTarget(s)).toEqual(other);
    expect(followedTarget(fold(LINK))).toBeNull();
  });
});

describe('isInsecureRemote', () => {
  it.each([
    ['http://192.168.1.5:8888', true],
    ['http://gateway.lan', true],
    ['http://127.0.0.1:8888', false],
    ['http://127.1.2.3', false],
    ['http://localhost:5173', false],
    ['http://app.localhost', false],
    ['http://[::1]:8888', false],
    ['https://192.168.1.5', false],
    ['not a url', false],
  ])('%s → %s', (url, insecure) => {
    expect(isInsecureRemote(url)).toBe(insecure);
  });
});

describe('splitUrl', () => {
  it('separates the origin from the rest', () => {
    expect(splitUrl('https://example.com/a?b')).toEqual({ origin: 'https://example.com', rest: '/a?b' });
    expect(splitUrl('about:blank')).toEqual({ origin: '', rest: 'about:blank' });
  });

  it('never lets userinfo or an explicit default port garble the origin', () => {
    expect(splitUrl('https://bank.com@evil.com/x')).toEqual({ origin: 'https://evil.com', rest: '/x' });
    expect(splitUrl('https://a.example:443/p#h')).toEqual({ origin: 'https://a.example', rest: '/p#h' });
  });
});

describe('isBrowserTool', () => {
  it('matches only the browser MCP server', () => {
    expect(isBrowserTool('browser/navigate_page')).toBe(true);
    expect(isBrowserTool('browserish/x')).toBe(false);
    expect(isBrowserTool('read_file')).toBe(false);
    expect(isBrowserTool(undefined)).toBe(false);
  });
});
