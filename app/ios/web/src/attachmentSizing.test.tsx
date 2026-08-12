import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { Transcript } from "./Transcript";
import type { PersistedState, Row, WireAttachment } from "./types";

/// How an attachment's box gets reserved — specifically, where its SIZE comes
/// from, which is not the same question for a raster blob and a vector.
///
/// A PNG carries its pixels, so `<img>.naturalWidth` is the truth and the
/// element itself can be asked once it decodes. An SVG carries none: WebKit
/// answers `naturalWidth` for one with the size it is laid out at right now, so
/// asking the element gives back whichever box the transcript already put it in
/// — the 12rem loading tile — and that number then rides the mirror to disk and
/// becomes the box the NEXT open reserves. A diagram that rendered the full
/// width of the column came back a third of it.
///
/// jsdom decodes nothing and lays out nothing, so both halves are faked: the
/// blob fetch resolves to a synthetic object URL, and the detached probe image
/// answers from a table keyed by that URL. What the suite can then hold is
/// exactly the wiring under test — which images get measured before they paint,
/// and what the bubble reserves as a result.

const { fetchBlob } = vi.hoisted(() => ({ fetchBlob: vi.fn() }));

vi.mock("./bridge", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./bridge")>()),
  blobObjectUrl: (blobId: string, mimeType: string) => fetchBlob(blobId, mimeType) as unknown,
}));

/// Intrinsic sizes the fake decoder hands back, keyed by object URL.
const intrinsic = new Map<string, [number, number]>();
/// Every URL a detached probe image was pointed at, in order.
let probed: string[] = [];

function objectUrl(blobId: string): string {
  return `blob:${blobId}`;
}

class FakeImage {
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  naturalWidth = 0;
  naturalHeight = 0;
  set src(value: string) {
    probed.push(value);
    const dims = intrinsic.get(value);
    queueMicrotask(() => {
      if (dims === undefined) {
        this.onerror?.();
        return;
      }
      this.naturalWidth = dims[0];
      this.naturalHeight = dims[1];
      this.onload?.();
    });
  }
}

function attachment(over: Partial<WireAttachment> = {}): WireAttachment {
  return {
    kind: "image",
    blob_id: "sha256:abc.tok",
    mime_type: "image/svg+xml",
    size: 24_190,
    filename: "chart.svg",
    ...over,
  };
}

function mirror(attachments: WireAttachment[], imageDims?: Record<string, [number, number]>) {
  const rows: Row[] = [
    { id: "m1", role: "assistant", content: "here it is", ordinal: 1, attachments },
  ];
  const state: PersistedState = {
    messages: rows,
    lastOrdinal: 1,
    oldestOrdinal: 1,
    hasMoreOlder: false,
    imageDims,
  };
  return state;
}

async function open(state: PersistedState): Promise<void> {
  render(
    <I18nextProvider i18n={i18n}>
      <Transcript restored={state} initialConnEpoch={0} />
    </I18nextProvider>,
  );
  // Two turns of the microtask queue plus a macrotask: the blob promise, the
  // probe's own `onload`, and the render each land on their own tick.
  await act(async () => {
    await new Promise((r) => setTimeout(r, 20));
  });
}

/// The reserved box the bubble ended up with — the two bare numbers styles.css
/// divides one by the other, or `null` when it reserved nothing at all.
function reservedBox(): [string, string] | null {
  const bubble = document.querySelector<HTMLElement>(".attachment-bubble");
  if (bubble === null || !bubble.classList.contains("sized")) return null;
  return [bubble.style.getPropertyValue("--img-w"), bubble.style.getPropertyValue("--img-h")];
}

beforeEach(() => {
  probed = [];
  intrinsic.clear();
  fetchBlob.mockImplementation((blobId: string) => Promise.resolve(objectUrl(blobId)));
  vi.stubGlobal("Image", FakeImage);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe(): void {}
      unobserve(): void {}
      disconnect(): void {}
    },
  );
  // The mirror write and every other outbound post degrade to console.log with
  // no `window.webkit`; keep the suite's output clean.
  vi.spyOn(console, "log").mockImplementation(() => {});
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("a vector attachment's box", () => {
  it("is the size it measures unconstrained, not the box it decoded in", async () => {
    intrinsic.set(objectUrl("sha256:abc.tok"), [1200, 400]);
    await open(mirror([attachment()]));
    expect(reservedBox()).toEqual(["1200", "400"]);
  });

  // The mirror is a file: it outlives the fix, so a thread opened today can
  // still be carrying the number its loading tile handed over months ago. A
  // stale entry has to be corrected on sight, not honoured for the life of the
  // thread.
  it("corrects a stale size recorded from an earlier loading tile", async () => {
    intrinsic.set(objectUrl("sha256:abc.tok"), [1200, 400]);
    await open(mirror([attachment()], { "sha256:abc": [192, 64] }));
    expect(reservedBox()).toEqual(["1200", "400"]);
  });

  // An SVG written as a bare `viewBox` has no intrinsic width for the
  // shrink-to-fit bubble to resolve, so with no box reserved it lays out at ZERO
  // — invisible, and untappable with it. The measurement is what hands it one.
  it("reserves a box even when nothing has painted yet", async () => {
    intrinsic.set(objectUrl("sha256:abc.tok"), [800, 400]);
    await open(mirror([attachment()]));
    expect(document.querySelector(".attachment-bubble.sized")).not.toBeNull();
  });

  it("keeps the mirror's size when the measurement fails", async () => {
    // No `intrinsic` entry: the probe errors, the way an evicted blob would.
    await open(mirror([attachment()], { "sha256:abc": [900, 300] }));
    expect(reservedBox()).toEqual(["900", "300"]);
  });
});

describe("a raster attachment", () => {
  // It carries its own pixels, so the element is the honest source and the extra
  // decode would buy nothing.
  it("is never probed", async () => {
    await open(mirror([attachment({ mime_type: "image/png", filename: "shot.png" })]));
    expect(probed).toEqual([]);
  });

  it("keeps reserving the box the mirror recorded", async () => {
    await open(
      mirror([attachment({ mime_type: "image/png", filename: "shot.png" })], {
        "sha256:abc": [1600, 400],
      }),
    );
    expect(reservedBox()).toEqual(["1600", "400"]);
  });
});
