import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { Transcript } from "./Transcript";
import type { PersistedState } from "./types";

/// A sync that FAILS on a thread with nothing to show. The empty thread's only
/// placeholder used to be gated on the request being in flight, so the failure
/// unmounted it and left blank paper — no message, no retry, and the failure
/// itself counted as stream traffic, pushing the 3-minute safety tick a full
/// interval out. Every open of a conversation this device had never rendered
/// went white the moment the network did.

vi.stubGlobal(
  "IntersectionObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);

const SAFETY_TICK_MS = 180_000;

let posts: Array<Record<string, unknown>> = [];

function syncPosts(): number {
  return posts.filter((p) => p.type === "sync").length;
}

async function advance(ms: number): Promise<void> {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

async function pushFrame(frame: Record<string, unknown>): Promise<void> {
  await act(async () => {
    window.baybo.pushFrame(JSON.stringify(frame));
  });
  await advance(20);
}

const failed = { kind: "sync_failed", error: "offline" };

function baselinePage(): Record<string, unknown> {
  return {
    kind: "sync_page",
    rows: [
      {
        kind: "message",
        id: "m1",
        role: "assistant",
        text: "hello",
        ordinal: 1,
        created_at: "2026-08-01T00:00:00Z",
      },
    ],
    since_ordinal: null,
    next_cursor: 1,
    rebased: false,
    oldest_ordinal: 1,
    has_more_older: false,
    compaction_points: [],
  };
}

async function mount(restored: PersistedState | null): Promise<void> {
  render(
    <I18nextProvider i18n={i18n}>
      <Transcript restored={restored} initialConnEpoch={1} />
    </I18nextProvider>,
  );
  await advance(20);
}

function failedButton(): HTMLButtonElement | null {
  return document.querySelector(".thread-load-failed");
}

beforeEach(() => {
  vi.useFakeTimers({
    toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"],
  });
  posts = [];
  vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
    if (args[0] === "[baybo bridge]" && typeof args[1] === "object" && args[1] !== null) {
      posts.push(args[1] as Record<string, unknown>);
    }
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("a failed sync on an empty thread", () => {
  it("says so instead of leaving the page blank", async () => {
    await mount(null);
    expect(document.querySelector(".thread-loading")).not.toBeNull();

    await pushFrame(failed);

    expect(document.querySelector(".thread-loading")).toBeNull();
    expect(failedButton()?.textContent).toBe(i18n.t("chat.loadThreadFailed"));
  });

  it("retries on a backoff until a page lands", async () => {
    await mount(null);
    const initial = syncPosts();

    await pushFrame(failed);
    await advance(1_900);
    expect(syncPosts()).toBe(initial);
    await advance(200);
    expect(syncPosts()).toBe(initial + 1);

    await pushFrame(failed);
    await advance(4_900);
    expect(syncPosts()).toBe(initial + 1);
    await advance(200);
    expect(syncPosts()).toBe(initial + 2);

    await pushFrame(baselinePage());
    expect(failedButton()).toBeNull();
    await advance(60_000);
    expect(syncPosts()).toBe(initial + 2);
  });

  it("retries at once when tapped", async () => {
    await mount(null);
    await pushFrame(failed);
    const before = syncPosts();

    await act(async () => {
      fireEvent.click(failedButton() as HTMLButtonElement);
    });

    expect(syncPosts()).toBe(before + 1);
    expect(failedButton()).toBeNull();
    expect(document.querySelector(".thread-loading")).not.toBeNull();
  });

  it("leaves a thread that has rows alone", async () => {
    await mount({
      messages: [{ id: "m1", role: "assistant", content: "cached", ordinal: 1 }],
      lastOrdinal: 1,
      oldestOrdinal: 1,
      hasMoreOlder: false,
    });
    const before = syncPosts();

    await pushFrame(failed);
    await advance(30_000);

    expect(failedButton()).toBeNull();
    expect(syncPosts()).toBe(before);
  });
});

describe("the safety tick", () => {
  it("is not pushed out by a failed sync", async () => {
    await mount({
      messages: [{ id: "m1", role: "assistant", content: "cached", ordinal: 1 }],
      lastOrdinal: 1,
      oldestOrdinal: 1,
      hasMoreOlder: false,
    });
    await advance(30_000);
    await pushFrame(failed);
    const before = syncPosts();

    await advance(SAFETY_TICK_MS - 30_000);

    expect(syncPosts()).toBe(before + 1);
  });
});
