import { describe, expect, it } from 'vitest';
import { buildWsUrl } from './wsUrl';
import { RECONNECT_BASE_MS, RECONNECT_MAX_MS, ReconnectBackoff } from './wsReconnect';

describe('buildWsUrl', () => {
  it('maps http to ws, replaces path and query, and carries the token', () => {
    expect(buildWsUrl('http://gw.test:8888/ignored?x=1', '/v1/browser/view/ws', 'a b')).toBe(
      'ws://gw.test:8888/v1/browser/view/ws?token=a+b',
    );
  });

  it('maps https to wss', () => {
    expect(buildWsUrl('https://gw.test', '/v1/channel-ws', 't')).toBe('wss://gw.test/v1/channel-ws?token=t');
  });
});

describe('ReconnectBackoff', () => {
  it('doubles from the base, caps, and resets', () => {
    const b = new ReconnectBackoff();
    const delays = Array.from({ length: 7 }, () => b.nextDelayMs());
    expect(delays).toEqual([1000, 2000, 4000, 8000, 16000, RECONNECT_MAX_MS, RECONNECT_MAX_MS]);
    b.reset();
    expect(b.nextDelayMs()).toBe(RECONNECT_BASE_MS);
  });
});
