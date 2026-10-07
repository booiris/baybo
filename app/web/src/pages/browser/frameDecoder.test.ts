import { describe, expect, it } from 'vitest';
import { FrameDecoder } from './frameDecoder';

interface Deferred {
  jpeg: Blob;
  resolve: (bitmap: string) => void;
  reject: (err: Error) => void;
}

function harness() {
  const calls: Deferred[] = [];
  const decoded: [number, string][] = [];
  const disposed: string[] = [];
  const decoder = new FrameDecoder<number, string>({
    decode: (jpeg) =>
      new Promise<string>((resolve, reject) => {
        calls.push({ jpeg, resolve, reject });
      }),
    onDecoded: (seq, bitmap) => decoded.push([seq, bitmap]),
    dispose: (bitmap) => disposed.push(bitmap),
  });
  const push = (seq: number) => decoder.push({ header: seq, jpeg: new Blob([String(seq)]) });
  return { decoder, calls, decoded, disposed, push };
}

const flush = () => new Promise<void>((r) => setTimeout(r, 0));

describe('FrameDecoder keep-latest', () => {
  it('keeps only the newest frame pending while a decode is in flight', async () => {
    const h = harness();
    h.push(1);
    h.push(2);
    h.push(3);
    h.push(4);
    expect(h.calls).toHaveLength(1);
    h.calls[0].resolve('b1');
    await flush();
    expect(h.decoded).toEqual([[1, 'b1']]);
    // Frames 2 and 3 were superseded and never decoded.
    expect(h.calls).toHaveLength(2);
    expect(await h.calls[1].jpeg.text()).toBe('4');
    h.calls[1].resolve('b4');
    await flush();
    expect(h.decoded).toEqual([
      [1, 'b1'],
      [4, 'b4'],
    ]);
    expect(h.calls).toHaveLength(2);
  });

  it('survives a failed decode and moves on to the pending frame', async () => {
    const h = harness();
    h.push(1);
    h.push(2);
    h.calls[0].reject(new Error('corrupt'));
    await flush();
    expect(h.calls).toHaveLength(2);
    h.calls[1].resolve('b2');
    await flush();
    expect(h.decoded).toEqual([[2, 'b2']]);
  });

  it('disposes a bitmap that lands after close and decodes nothing more', async () => {
    const h = harness();
    h.push(1);
    h.push(2);
    h.decoder.close();
    h.calls[0].resolve('b1');
    await flush();
    expect(h.decoded).toEqual([]);
    expect(h.disposed).toEqual(['b1']);
    h.push(3);
    expect(h.calls).toHaveLength(1);
  });
});
