import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { FrameHeader, ViewerDown } from './browserViewTypes';
import {
  BrowserViewConnection,
  parseFramePayload,
  parseViewerDown,
  type BrowserViewConnStatus,
  type ScreencastFrame,
  type VisibilitySource,
} from './browserViewWs';

const HEADER: FrameHeader = {
  target_id: 'T1',
  browser_gen: 2,
  seq: 7,
  captured_at_ms: 1000.5,
  device_width: 1280,
  device_height: 800,
  offset_top: 0,
  page_scale_factor: 1,
  scroll_offset_x: 0,
  scroll_offset_y: 120,
};

function framePayload(header: unknown, jpeg: Uint8Array): ArrayBuffer {
  const hdr = new TextEncoder().encode(JSON.stringify(header));
  const buf = new Uint8Array(4 + hdr.length + jpeg.length);
  new DataView(buf.buffer).setUint32(0, hdr.length, false);
  buf.set(hdr, 4);
  buf.set(jpeg, 4 + hdr.length);
  return buf.buffer;
}

describe('parseFramePayload', () => {
  it('splits the BE header length, the JSON header and the JPEG', async () => {
    const jpeg = new Uint8Array([0xff, 0xd8, 0xff, 0xd9]);
    const frame = parseFramePayload(framePayload(HEADER, jpeg));
    expect(frame?.header).toEqual(HEADER);
    expect(frame?.jpeg.type).toBe('image/jpeg');
    expect(new Uint8Array(await frame!.jpeg.arrayBuffer())).toEqual(jpeg);
  });

  it('rejects a header length past the end of the message', () => {
    const buf = framePayload(HEADER, new Uint8Array());
    expect(parseFramePayload(buf.slice(0, buf.byteLength - 1))).toBeNull();
  });

  it('rejects a message shorter than the length prefix', () => {
    expect(parseFramePayload(new ArrayBuffer(3))).toBeNull();
  });

  it('rejects a header that is not JSON or not a FrameHeader', () => {
    const notJson = new Uint8Array([0, 0, 0, 2, 0x7b, 0x7b]);
    expect(parseFramePayload(notJson.buffer)).toBeNull();
    expect(parseFramePayload(framePayload({ ...HEADER, seq: '7' }, new Uint8Array([1])))).toBeNull();
  });
});

describe('parseViewerDown', () => {
  it('passes known types and drops unknown or malformed ones', () => {
    expect(parseViewerDown('{"type":"pong"}')).toEqual({ type: 'pong' });
    expect(parseViewerDown('{"type":"future_thing"}')).toBeNull();
    expect(parseViewerDown('not json')).toBeNull();
    expect(parseViewerDown('[1]')).toBeNull();
  });
});

class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readyState = FakeWebSocket.CONNECTING;
  binaryType: BinaryType = 'blob';
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  readonly sent: string[] = [];

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.(new Event('open'));
  }

  receiveText(msg: ViewerDown): void {
    this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(msg) }));
  }

  receiveBinary(data: ArrayBuffer): void {
    this.onmessage?.(new MessageEvent('message', { data }));
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.(new CloseEvent('close', { code: 1006 }));
  }
}

class FakeVisibility implements VisibilitySource {
  visibilityState: DocumentVisibilityState = 'visible';
  private listeners = new Set<() => void>();
  addEventListener(_type: 'visibilitychange', l: () => void): void {
    this.listeners.add(l);
  }
  removeEventListener(_type: 'visibilitychange', l: () => void): void {
    this.listeners.delete(l);
  }
  set(state: DocumentVisibilityState): void {
    this.visibilityState = state;
    for (const l of this.listeners) l();
  }
}

const PING_MS = 15_000;
const LINK: ViewerDown = {
  type: 'link',
  up: true,
  link_epoch: 1,
  unavailable: null,
  ping_interval_ms: PING_MS,
};

describe('BrowserViewConnection', () => {
  let conns: BrowserViewConnection[] = [];
  let statuses: BrowserViewConnStatus[];
  let messages: ViewerDown[];
  let frames: ScreencastFrame[];
  let visibility: FakeVisibility;

  function connect(): BrowserViewConnection {
    const c = new BrowserViewConnection({
      baseUrl: 'http://gw.test',
      token: 'tok',
      onMessage: (m) => messages.push(m),
      onFrame: (f) => frames.push(f),
      onStatus: (s) => statuses.push(s),
      visibility,
    });
    conns.push(c);
    return c;
  }

  const latest = () => FakeWebSocket.instances[FakeWebSocket.instances.length - 1];
  const pings = (ws: FakeWebSocket) => ws.sent.filter((s) => s === '{"type":"ping"}').length;

  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.instances = [];
    vi.stubGlobal('WebSocket', FakeWebSocket);
    statuses = [];
    messages = [];
    frames = [];
    visibility = new FakeVisibility();
  });

  afterEach(() => {
    for (const c of conns) c.close();
    conns = [];
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('connects with the token, forwards text and binary, and swallows pongs', () => {
    connect();
    const ws = latest();
    expect(ws.url).toBe('ws://gw.test/v1/browser/view/ws?token=tok');
    expect(ws.binaryType).toBe('arraybuffer');
    ws.open();
    ws.receiveText(LINK);
    ws.receiveText({ type: 'pong' });
    ws.receiveBinary(framePayload(HEADER, new Uint8Array([1, 2])));
    expect(statuses.map((s) => s.state)).toEqual(['connecting', 'connected']);
    expect(messages).toEqual([LINK]);
    expect(frames.map((f) => f.header)).toEqual([HEADER]);
  });

  it('pings on the announced cadence and not before the link message', () => {
    connect();
    const ws = latest();
    ws.open();
    vi.advanceTimersByTime(PING_MS * 3);
    expect(pings(ws)).toBe(0);
    ws.receiveText(LINK);
    for (let i = 1; i <= 3; i++) {
      vi.advanceTimersByTime(PING_MS);
      ws.receiveText({ type: 'pong' });
      expect(pings(ws)).toBe(i);
    }
  });

  it('keeps a static page connected purely by ping/pong', () => {
    connect();
    const ws = latest();
    ws.open();
    ws.receiveText(LINK);
    for (let i = 0; i < 10; i++) {
      vi.advanceTimersByTime(PING_MS);
      ws.receiveText({ type: 'pong' });
    }
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(ws.readyState).toBe(FakeWebSocket.OPEN);
  });

  it('closes a silent socket and reconnects with backoff', () => {
    connect();
    const ws = latest();
    ws.open();
    ws.receiveText(LINK);
    // Two intervals of silence pass the liveness budget on the third tick.
    vi.advanceTimersByTime(PING_MS * 3);
    expect(ws.readyState).toBe(FakeWebSocket.CLOSED);
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 1000 });
    vi.advanceTimersByTime(999);
    expect(FakeWebSocket.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it('backs off on repeated failures and resets once the link message arrives', () => {
    connect();
    latest().close();
    vi.advanceTimersByTime(1000);
    latest().close();
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 2000 });
    vi.advanceTimersByTime(2000);
    latest().open();
    latest().receiveText(LINK);
    latest().close();
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 1000 });
  });

  it('keeps backing off when a full gateway opens, errors and closes', () => {
    connect();
    const full = () => {
      latest().open();
      latest().receiveText({ type: 'error', code: 'too_many_viewers' });
      latest().close();
    };
    full();
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 1000 });
    vi.advanceTimersByTime(1000);
    full();
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 2000 });
    vi.advanceTimersByTime(2000);
    full();
    expect(statuses[statuses.length - 1]).toMatchObject({ state: 'disconnected', retryInMs: 4000 });
  });

  it('disconnects while hidden and reconnects when visible', () => {
    connect();
    const first = latest();
    first.open();
    first.receiveText(LINK);
    visibility.set('hidden');
    expect(first.readyState).toBe(FakeWebSocket.CLOSED);
    expect(statuses[statuses.length - 1]).toEqual({ state: 'hidden' });
    vi.advanceTimersByTime(60_000);
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(pings(first)).toBe(0);
    visibility.set('visible');
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(statuses[statuses.length - 1]).toEqual({ state: 'connecting' });
  });

  it('does not connect while starting hidden, and close() stops everything', () => {
    visibility.visibilityState = 'hidden';
    const c = connect();
    expect(FakeWebSocket.instances).toHaveLength(0);
    c.close();
    visibility.set('visible');
    expect(FakeWebSocket.instances).toHaveLength(0);
  });
});
