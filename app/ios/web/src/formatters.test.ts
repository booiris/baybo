import type { TFunction } from "i18next";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clampVideoRatio,
  formatBytes,
  formatTime,
  formatTimestampShort,
  isVectorImage,
  isVideoAttachment,
  ordinalFromMessageId,
  rowOrdinal,
  splitForMiddleEllipsis,
  typeLabel,
  workStepKey,
} from "./Transcript";
import type { WireAttachment, WorkStep } from "./types";
import { approvalLabel, formatDuration, workedLabel } from "./WorkBlock";

/// Booting i18next would assert the English copy; the contract these helpers
/// actually own is the KEY plus its interpolation values.
const t = ((key: string, values?: unknown) =>
  values === undefined ? key : `${key}:${JSON.stringify(values)}`) as unknown as TFunction;

function attachment(over: Partial<WireAttachment> = {}): WireAttachment {
  return { kind: "file", blob_id: "sha256:ab.tok", mime_type: "application/pdf", size: 1024, ...over };
}

describe("formatBytes", () => {
  it.each([
    [0, "0 B"],
    [812, "812 B"],
    [1023, "1023 B"],
    [1024, "1 KB"],
    [24 * 1024, "24 KB"],
    [1024 * 1024 - 1, "1024 KB"],
    [1024 * 1024, "1.0 MB"],
    [2.3 * 1024 * 1024, "2.3 MB"],
    [140 * 1024 * 1024, "140 MB"],
  ])("%i → %s", (bytes, expected) => {
    expect(formatBytes(bytes)).toBe(expected);
  });
});

describe("formatTime", () => {
  it.each([
    [0, "0:00"],
    [7, "0:07"],
    [59, "0:59"],
    [83, "1:23"],
    [599, "9:59"],
    [3599, "59:59"],
    [3600, "1:00:00"],
    [3725, "1:02:05"],
    [-5, "0:00"],
  ])("%i s → %s", (seconds, expected) => {
    expect(formatTime(seconds)).toBe(expected);
  });
});

/// Built from LOCAL date parts, since the formatter reads the local clock — a
/// UTC literal would drift into the previous/next day on a runner east or west
/// of the fixture's assumption.
function localIso(y: number, mo: number, d: number, h: number, mi: number): string {
  return new Date(y, mo - 1, d, h, mi).toISOString();
}

describe("formatTimestampShort", () => {
  afterEach(() => vi.useRealTimers());

  it("shows only HH:MM for a message from today — the date would be noise under every bubble", () => {
    vi.useFakeTimers().setSystemTime(new Date(2026, 6, 23, 18, 30));
    expect(formatTimestampShort(localIso(2026, 7, 23, 9, 5))).toBe("09:05");
  });

  it("prefixes MM-DD once the row is from another day, so a long thread stays readable", () => {
    vi.useFakeTimers().setSystemTime(new Date(2026, 6, 23, 0, 30));
    expect(formatTimestampShort(localIso(2026, 7, 22, 23, 59))).toBe("07-22 23:59");
  });

  it("is not fooled by the same day-of-month a year earlier", () => {
    vi.useFakeTimers().setSystemTime(new Date(2026, 6, 23, 12, 0));
    expect(formatTimestampShort(localIso(2025, 7, 23, 12, 0))).toBe("07-23 12:00");
  });

  it("renders nothing for an unparseable stamp rather than 'Invalid Date'", () => {
    expect(formatTimestampShort("not a time")).toBe("");
  });
});

describe("typeLabel", () => {
  it.each([
    ["the filename's extension — the most honest source", "report.pdf", "application/pdf", "PDF"],
    ["a 4-char extension", "notes.docx", "application/msword", "DOCX"],
    ["an over-long extension falls back to the mime", "archive.tarball", "application/gzip", "GZIP"],
    ["a dotfile is not an extension", ".env", "text/plain", "PLAIN"],
    ["a nameless blob uses the mime subtype", undefined, "image/png", "PNG"],
    ["the +xml suffix is stripped", undefined, "image/svg+xml", "SVG"],
    ["a mime parameter is stripped", undefined, "text/csv;charset=utf-8", "CSV"],
    [
      "a vnd.* vendor path collapses to its last segment",
      undefined,
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
      "DOCUMENT",
    ],
  ])("uses %s", (_why, filename, mime, expected) => {
    expect(typeLabel(attachment({ filename, mime_type: mime }))).toBe(expected);
  });
});

describe("splitForMiddleEllipsis", () => {
  it("leaves a short name whole — it takes the full width and needs no pinned tail", () => {
    expect(splitForMiddleEllipsis("notes.pdf")).toEqual(["notes.pdf", ""]);
  });

  it("pins the last 10 chars so `…-Q3-final.pdf` never loses the part that identifies it", () => {
    const [head, tail] = splitForMiddleEllipsis("quarterly-revenue-Q3-final.pdf");
    expect(tail).toBe("-final.pdf");
    expect(head + tail).toBe("quarterly-revenue-Q3-final.pdf");
  });

  it("does not split exactly at the boundary (20 chars)", () => {
    expect(splitForMiddleEllipsis("a".repeat(20))).toEqual(["a".repeat(20), ""]);
    expect(splitForMiddleEllipsis("a".repeat(21))[1]).toHaveLength(10);
  });
});

describe("clampVideoRatio", () => {
  it.each([
    ["a 9:16 portrait column clamps up to 3:4", 9 / 16, 3 / 4],
    ["an ultra-wide strip clamps down to 16:9", 32 / 9, 16 / 9],
    ["a 4:3 tile passes through", 4 / 3, 4 / 3],
    ["the bounds are inclusive", 16 / 9, 16 / 9],
  ])("%s", (_why, ratio, expected) => {
    expect(clampVideoRatio(ratio)).toBeCloseTo(expected, 10);
  });
});

describe("isVideoAttachment", () => {
  it("elects the tile by mime — video has no wire kind of its own", () => {
    expect(isVideoAttachment(attachment({ kind: "file", mime_type: "video/mp4" }))).toBe(true);
  });

  it.each([
    ["a plain file", "file" as const, "application/pdf"],
    ["an audio blob", "audio" as const, "audio/mpeg"],
    ["an image", "image" as const, "image/png"],
  ])("rejects %s", (_why, kind, mime) => {
    expect(isVideoAttachment(attachment({ kind, mime_type: mime }))).toBe(false);
  });
});

describe("isVectorImage", () => {
  it("elects the pre-paint measurement path — an SVG has no pixels to ask for", () => {
    expect(isVectorImage(attachment({ kind: "image", mime_type: "image/svg+xml" }))).toBe(true);
  });

  it("survives a mime carrying parameters", () => {
    expect(
      isVectorImage(attachment({ kind: "image", mime_type: "image/SVG+XML; charset=utf-8" })),
    ).toBe(true);
  });

  it.each([
    ["a raster image", "image" as const, "image/png"],
    // A `.svg` the gateway bucketed as a plain file renders as a named chip, not
    // an <img> — there is nothing to measure.
    ["an svg sent as a file", "file" as const, "image/svg+xml"],
    ["an audio blob", "audio" as const, "audio/mpeg"],
  ])("rejects %s", (_why, kind, mime) => {
    expect(isVectorImage(attachment({ kind, mime_type: mime }))).toBe(false);
  });
});

describe("ordinalFromMessageId", () => {
  it.each([
    ["m12", 12],
    ["m0", 0],
    ["w12", null],
    ["n3", null],
    ["u1-9f2", null],
    ["pm-1", null],
    ["m12x", null],
    [`m${Number.MAX_SAFE_INTEGER}0`, null],
  ])("%s → %s", (id, expected) => {
    expect(ordinalFromMessageId(id)).toBe(expected);
  });
});

describe("rowOrdinal", () => {
  it.each([
    ["m12", 12],
    ["w12", 12],
    ["n3", null],
    ["u1-9f2", null],
  ])("%s → %s — a work block is ordinal-addressed too", (id, expected) => {
    expect(rowOrdinal(id)).toBe(expected);
  });
});

describe("workStepKey", () => {
  it("keys a tool step by its call id — stable across the live and reconstructed shapes", () => {
    const step: WorkStep = { kind: "tool", callId: "c9", label: "Bash", status: "running" };
    expect(workStepKey(step)).toBe("tool:c9");
  });

  it("keys a text step by kind + text, so two kinds carrying the same text stay distinct", () => {
    expect(workStepKey({ kind: "reasoning", text: "hmm" })).toBe("reasoning:hmm");
    expect(workStepKey({ kind: "prose", text: "hmm" })).toBe("prose:hmm");
  });
});

describe("formatDuration", () => {
  it.each([
    ["seconds under a minute", 5_000, "chat.durS", { s: 5 }],
    ["the last second before the minute boundary", 59_000, "chat.durS", { s: 59 }],
    ["exactly one minute crosses into the m form", 60_000, "chat.durM", { m: 1 }],
    ["minutes and seconds", 90_000, "chat.durMS", { m: 1, s: 30 }],
    ["a whole minute drops the seconds", 120_000, "chat.durM", { m: 2 }],
    ["hours and minutes", 5_400_000, "chat.durHM", { h: 1, m: 30 }],
    ["a whole hour drops the minutes", 3_600_000, "chat.durH", { h: 1 }],
    ["a 60-minute rounding carry rolls the hour", 7_170_000, "chat.durH", { h: 2 }],
  ])("%s", (_why, ms, key, values) => {
    expect(formatDuration(t, ms)).toBe(`${key}:${JSON.stringify(values)}`);
  });
});

describe("workedLabel", () => {
  it("says just 'Worked' for a turn too short to time", () => {
    expect(workedLabel(t, undefined)).toBe("chat.worked");
    expect(workedLabel(t, 500)).toBe("chat.worked");
  });

  it("interpolates the humanized duration once there is one to show", () => {
    const label = workedLabel(t, 7_000);
    expect(label).toContain("chat.workedFor");
    expect(label).toContain("chat.durS");
  });
});

describe("approvalLabel", () => {
  it("reads 'waiting' while the gate holds the call — the badge outranks any verdict", () => {
    expect(
      approvalLabel(t, { kind: "tool", callId: "c", label: "Bash", status: "running", awaitingApproval: "p1" }),
    ).toBe("chat.approvalWaiting");
  });

  it.each([
    ["approve", "chat.approvalApproved"],
    ["approve_always", "chat.approvalApprovedAlways"],
    ["deny", "chat.approvalDenied"],
  ])("labels a %s verdict (it is persisted, so a reload re-labels the step)", (decision, key) => {
    expect(approvalLabel(t, { kind: "tool", callId: "c", label: "Bash", status: "ok", approval: decision })).toBe(key);
  });

  it("labels nothing for a call that never prompted, or for a non-tool step", () => {
    expect(approvalLabel(t, { kind: "tool", callId: "c", label: "Bash", status: "ok" })).toBeNull();
    expect(approvalLabel(t, { kind: "reasoning", text: "hmm" })).toBeNull();
  });
});
