// Keep-latest JPEG decoding. Frames can arrive faster than they decode (a
// slow machine, a busy main thread); queueing them would only grow latency,
// so while one decode is in flight a single pending slot holds the NEWEST
// frame and every frame it replaces is dropped undecoded.

export interface DecoderFrame<H> {
  header: H;
  jpeg: Blob;
}

export interface FrameDecoderOptions<H, B> {
  decode: (jpeg: Blob) => Promise<B>;
  /** Receives ownership of `bitmap`. */
  onDecoded: (header: H, bitmap: B) => void;
  /** Releases a bitmap that decoded after the decoder was closed. */
  dispose: (bitmap: B) => void;
}

export class FrameDecoder<H, B> {
  private inFlight = false;
  private pending: DecoderFrame<H> | null = null;
  private closed = false;

  constructor(private readonly opts: FrameDecoderOptions<H, B>) {}

  push(frame: DecoderFrame<H>): void {
    if (this.closed) return;
    if (this.inFlight) {
      this.pending = frame;
      return;
    }
    this.run(frame);
  }

  close(): void {
    this.closed = true;
    this.pending = null;
  }

  private run(frame: DecoderFrame<H>): void {
    this.inFlight = true;
    this.opts.decode(frame.jpeg).then(
      (bitmap) => {
        if (this.closed) this.opts.dispose(bitmap);
        else this.opts.onDecoded(frame.header, bitmap);
        this.next();
      },
      // A corrupt JPEG costs one frame, not the stream.
      () => this.next(),
    );
  }

  private next(): void {
    this.inFlight = false;
    const pending = this.pending;
    this.pending = null;
    if (pending && !this.closed) this.run(pending);
  }
}

export function createBitmapDecoder<H>(
  onDecoded: (header: H, bitmap: ImageBitmap) => void,
): FrameDecoder<H, ImageBitmap> {
  return new FrameDecoder<H, ImageBitmap>({
    decode: (jpeg) => createImageBitmap(jpeg),
    onDecoded,
    dispose: (bitmap) => bitmap.close(),
  });
}
