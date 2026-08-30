// A subscribed-channel send is one wire transaction: Register, then Subscribe,
// then Message. This pins the return-to-Chat outbox recovery path, where the
// recovered session can exist before `/chat` has selected an active URL session.

import { decode, encode } from '@msgpack/msgpack';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ChatWs, type ConnectionStatus, type Frame } from './chatWs';

const SESSION_ID = 'session-outbox';

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
  readonly sent: Uint8Array[] = [];

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.(new Event('open'));
  }

  receive(frame: Frame): void {
    const bytes = encode(frame);
    const data = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    this.onmessage?.(new MessageEvent('message', { data }));
  }

  send(data: Uint8Array): void {
    this.sent.push(data.slice());
  }

  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.(new CloseEvent('close', { code: 1000 }));
  }
}

function frames(socket: FakeWebSocket): Frame[] {
  return socket.sent.map((bytes) => decode(bytes) as Frame);
}

function messageKinds(socket: FakeWebSocket): Frame['kind'][] {
  return frames(socket).map((frame) => frame.kind);
}

describe('ChatWs subscribed send barrier', () => {
  const clients: ChatWs[] = [];

  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal('WebSocket', FakeWebSocket);
  });

  afterEach(() => {
    for (const client of clients.splice(0)) client.close();
    vi.unstubAllGlobals();
  });

  it('queues Subscribe before a connected-edge outbox recovery Message', () => {
    const statuses: ConnectionStatus[] = [];
    let client: ChatWs;
    client = new ChatWs({
      baseUrl: 'http://gateway.test',
      adminToken: 'token',
      initialSessionIds: [],
      onFrame: () => {},
      onStatus: (status) => {
        statuses.push(status);
        if (status.state !== 'connected') return;
        expect(
          client.sendMessage({
            sessionId: SESSION_ID,
            userId: 'web-operator',
            content: 'recover me',
            clientMsgId: 'outbox-key',
          }),
        ).toBe(true);
      },
    });
    clients.push(client);

    const socket = FakeWebSocket.instances[0];
    socket.open();
    socket.receive({ kind: 'register_ack', ok: true, reason: null });

    expect(statuses.map((status) => status.state)).toEqual(['connecting', 'connected']);
    expect(messageKinds(socket)).toEqual(['register', 'subscribe', 'message']);
    expect(frames(socket)[1]).toEqual({ kind: 'subscribe', session_id: SESSION_ID });
  });

  it('holds a send until RegisterAck and then subscribes it on retry', () => {
    const client = new ChatWs({
      baseUrl: 'http://gateway.test',
      adminToken: 'token',
      onFrame: () => {},
    });
    clients.push(client);
    const socket = FakeWebSocket.instances[0];
    socket.open();

    expect(
      client.sendMessage({
        sessionId: SESSION_ID,
        userId: 'web-operator',
        content: 'too early',
        clientMsgId: 'early-key',
      }),
    ).toBe(false);
    expect(messageKinds(socket)).toEqual(['register']);

    socket.receive({ kind: 'register_ack', ok: true, reason: null });
    expect(messageKinds(socket)).toEqual(['register', 'subscribe']);
    expect(
      client.sendMessage({
        sessionId: SESSION_ID,
        userId: 'web-operator',
        content: 'retry',
        clientMsgId: 'early-key',
      }),
    ).toBe(true);
    expect(messageKinds(socket)).toEqual(['register', 'subscribe', 'message']);
  });

  it('applies the same subscription barrier to an atomic message batch', () => {
    const client = new ChatWs({
      baseUrl: 'http://gateway.test',
      adminToken: 'token',
      onFrame: () => {},
    });
    clients.push(client);
    const socket = FakeWebSocket.instances[0];
    socket.open();
    socket.receive({ kind: 'register_ack', ok: true, reason: null });

    expect(
      client.sendMessages(SESSION_ID, [
        { content: 'one', clientMsgId: 'batch-1' },
        { content: 'two', clientMsgId: 'batch-2' },
      ]),
    ).toBe(true);
    expect(messageKinds(socket)).toEqual(['register', 'subscribe', 'messages']);
  });
});
