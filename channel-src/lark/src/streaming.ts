import type { Logger } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

/**
 * Wraps `LarkChannel.stream()` so a per-(bot, chat) streaming card
 * stays open across many `onAgentDelta` calls and a final `onMessage`.
 *
 * The Lark SDK's `MarkdownStreamProducer` is a one-shot async function:
 * it receives a `MarkdownStreamController`, calls `setContent(full)`
 * one or more times, and the stream finalises when its returned
 * promise resolves. The SDK already owns the throttle
 * (`OutboundConfig.streamThrottleMs` / `streamThrottleChars`) and the
 * underlying CardKit `cards.stream` orchestration; we only need a
 * latch so the producer's loop blocks until the next chunk arrives.
 *
 * Latch design: we hold a single "tick" promise that the producer
 * awaits between flushes. Each `append`/`setContent`/`finish` resolves
 * it; the producer immediately replaces it with a fresh one before
 * doing any async work, so a wake that lands during `setContent`
 * still gets observed on the next loop turn (without losing wakes
 * across the `await` boundary). Promise-resolve idempotency means
 * stacked wakes collapse to one — desirable, since the buffer is
 * eventually consistent.
 *
 * Failure mode: the SDK's `stream()` may reject (rate-limited,
 * network blip). The session captures the rejection on
 * `streamPromise` so the next caller's `finish()` re-throws; we don't
 * propagate from `append`/`setContent` because those are
 * fire-and-forget on the delta path, where logging is more helpful
 * than throwing.
 */
interface ToolIndicator {
  tool: string;
  paramsPreview: string;
  status: "running" | "ok" | "error";
}

export class LarkStreamingSession {
  private buf = "";
  private done = false;
  private tick: Promise<void>;
  private wake: () => void;
  private streamError: unknown = null;
  private readonly streamPromise: Promise<void>;
  // Tool-use indicators rendered as a header above the agent's prose.
  // Insertion-ordered so users see the call sequence; entries don't
  // disappear when a call completes — the icon swaps in place so the
  // turn's tool history stays visible.
  private readonly toolCalls = new Map<string, ToolIndicator>();

  constructor(
    channel: lark.LarkChannel,
    chatId: string,
    private readonly logger: Logger,
  ) {
    // Seed the latch so the producer can immediately await it. The
    // declarations above leave `tick`/`wake` definite-assigned via
    // the helper below; TS just can't see through it on the first
    // call site.
    [this.tick, this.wake] = makeTick();

    this.streamPromise = channel
      .stream(chatId, {
        markdown: async (controller) => {
          // Each iteration flushes the *current* buffer, then re-flushes
          // if the buffer changed during the in-flight `setContent`
          // (typical when `finish(text)` lands while we're mid-flush).
          // Replacing `tick` BEFORE the async `setContent` ensures
          // wakes that arrive during the flush survive into the next
          // loop turn rather than getting lost. The exit branch
          // includes the same "final flush" guard so the terminal
          // buffer always lands on the card even when `done` flips
          // between two adjacent flushes.
          let lastFlushed = "";
          while (true) {
            await this.tick;
            [this.tick, this.wake] = makeTick();
            const composed = this.compose();
            if (composed !== lastFlushed) {
              lastFlushed = composed;
              await controller.setContent(composed);
            }
            if (this.done) {
              const final = this.compose();
              if (final !== lastFlushed) {
                await controller.setContent(final);
              }
              return;
            }
          }
        },
      })
      .then(
        () => {},
        (err: unknown) => {
          this.streamError = err;
          this.logger.debug(
            `lark streaming session failed: ${String(err)}`,
          );
        },
      );
  }

  append(text: string): void {
    if (this.done) return;
    if (text.length === 0) return;
    this.buf += text;
    this.wake();
  }

  setContent(text: string): void {
    if (this.done) return;
    this.buf = text;
    this.wake();
  }

  setToolCallRunning(callId: string, tool: string, paramsPreview: string): void {
    if (this.done) return;
    this.toolCalls.set(callId, { tool, paramsPreview, status: "running" });
    this.wake();
  }

  setToolCallCompleted(callId: string, errored: boolean): void {
    if (this.done) return;
    const existing = this.toolCalls.get(callId);
    if (!existing) return;
    existing.status = errored ? "error" : "ok";
    this.wake();
  }

  private compose(): string {
    if (this.toolCalls.size === 0) return this.buf;
    const lines: string[] = [];
    for (const ind of this.toolCalls.values()) {
      const icon =
        ind.status === "running" ? "🔧" : ind.status === "ok" ? "✓" : "✗";
      // params preview is already byte-capped upstream; squashing
      // newlines keeps the indicator on one line so the card stays
      // tidy even when the LLM passed multi-line JSON.
      const oneLine = ind.paramsPreview.replace(/\s+/g, " ");
      lines.push(`${icon} **${ind.tool}**  \`${oneLine}\``);
    }
    const header = lines.join("\n");
    return this.buf.length === 0 ? header : `${header}\n\n${this.buf}`;
  }

  /**
   * Mark the session terminal and wait for the SDK's `stream()` to
   * finalise the card. If the underlying `stream()` rejected we
   * re-throw the error here so the caller can decide whether to fall
   * back to a plain `sendText`.
   */
  async finish(finalText?: string): Promise<void> {
    if (!this.done) {
      if (finalText !== undefined) this.buf = finalText;
      this.done = true;
      this.wake();
    }
    await this.streamPromise;
    if (this.streamError !== null) {
      throw this.streamError instanceof Error
        ? this.streamError
        : new Error(String(this.streamError));
    }
  }
}

function makeTick(): [Promise<void>, () => void] {
  let wake: () => void = () => {};
  const tick = new Promise<void>((resolve) => {
    wake = resolve;
  });
  return [tick, wake];
}
